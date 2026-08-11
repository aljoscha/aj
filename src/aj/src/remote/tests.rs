//! The HTTP transport end to end (spec 6.1-6.11, and the equivalence
//! harness of 11.2).
//!
//! Everything here runs a real host over the scripted provider behind a real
//! loopback server, so the bytes under test are the bytes a remote client
//! sees. Three groups:
//!
//! - the identity gate, against a faked whois resolver,
//! - the routes: the handshake, the reads, the mutations, and the status
//!   vocabulary of spec 6.1,
//! - reducer equivalence: one client attached in process as the oracle and
//!   one through HTTP, folded with the same [`SessionClient`] and compared on
//!   [`CanonicalState`], including the fault-injection variant that cuts the
//!   stream and re-attaches with a cursor.
//!
//! Every wait is bounded by [`DEADLINE`], so a wedged host fails a test
//! instead of hanging CI.

use std::collections::BTreeSet;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_app::chat::{ChatState, SubAgentStatus};
use aj_app::client::SessionClient;
use aj_app::host::{AttachRequest, Attachment, CommandOutcome, HostSetup, SessionHost};
use aj_app::session_setup::RunConfigSnapshot;
use aj_app::settings::ConfigLayers;
use aj_app::test_support::{
    CanonicalEntry, CanonicalState, assert_canonical_eq, assert_no_dangling,
    finalized_text_message, scripted_model_info,
};
use aj_conf::{Config, ConfigLayer};
use aj_models::auth::AuthStorage;
use aj_models::registry::ModelInfo;
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use aj_models::types::{AssistantContent, AssistantMessage, StopReason, ToolCall};
use aj_session::{ConversationPersistence, ThreadFilter};
use aj_wire::{
    CancelRequest, CompactRequest, CreateSessionRequest, Cursor, DecodedFrame, ErrorResponse,
    Frame, HeadRequest, ModelSelection, PROTOCOL_VERSION, PromptInput, PromptRequest,
    QueueOperation, QueueRequest, QueueState, SessionSettings, SettingsRequest, SteerRequest,
    TagRequest, TaskTable,
};
use async_trait::async_trait;
use reqwest::StatusCode;
use tempfile::TempDir;

use super::*;
use crate::control::{Control, ControlError, ControlFrame};
use crate::remote::identity::{
    AJ_CONTROL_CAPABILITY, IdentityError, PeerIdentity, WhoisResolver, peer_identity_from_whois,
};

/// Every wait in this file is bounded by this.
const DEADLINE: Duration = Duration::from_secs(20);

/// How long a settled stream has to prove it is not settled after all.
///
/// Longer than the host's own coalescing ticks (the `list` publisher's is
/// 200ms), short enough that a comparison at quiescence stays cheap.
const QUIET: Duration = Duration::from_millis(300);

/// Await `future`, failing the test rather than hanging.
async fn bounded<T>(what: &str, future: impl Future<Output = T>) -> T {
    match tokio::time::timeout(DEADLINE, future).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for {what}"),
    }
}

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("a socket address")
}

// ---------------------------------------------------------------------------
// The identity gate (spec 6.11)
// ---------------------------------------------------------------------------

/// A resolver with a fixed answer, recording the peers it was asked about.
struct FakeWhois {
    answer: Result<PeerIdentity, String>,
    asked: StdMutex<Vec<SocketAddr>>,
}

impl FakeWhois {
    fn resolving(identity: PeerIdentity) -> Arc<Self> {
        Arc::new(Self {
            answer: Ok(identity),
            asked: StdMutex::new(Vec::new()),
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            answer: Err("tailscaled is not running".to_string()),
            asked: StdMutex::new(Vec::new()),
        })
    }

    /// The same resolver as a trait object, so a test can keep the concrete
    /// handle and still hand the gate its resolver.
    fn resolver(self: &Arc<Self>) -> Arc<dyn WhoisResolver> {
        let concrete: Arc<Self> = Arc::clone(self);
        concrete
    }

    fn asked(&self) -> Vec<SocketAddr> {
        self.asked.lock().expect("asked mutex poisoned").clone()
    }
}

#[async_trait]
impl WhoisResolver for FakeWhois {
    async fn resolve(&self, peer: SocketAddr) -> Result<PeerIdentity, IdentityError> {
        self.asked.lock().expect("asked mutex poisoned").push(peer);
        self.answer
            .clone()
            .map_err(|reason| IdentityError::Lookup(reason.into()))
    }
}

/// A peer with a user behind it.
fn user_peer(login: &str) -> PeerIdentity {
    PeerIdentity {
        login: Some(login.to_string()),
        node: "laptop".to_string(),
        tags: Vec::new(),
        capabilities: BTreeSet::new(),
    }
}

/// A tagged node, which has no login at all.
fn tagged_peer(capabilities: &[&str]) -> PeerIdentity {
    PeerIdentity {
        login: None,
        node: "aj-vm".to_string(),
        tags: vec!["tag:aj-host".to_string()],
        capabilities: capabilities.iter().map(|cap| cap.to_string()).collect(),
    }
}

#[test]
fn auth_modes_round_trip_their_names() {
    for (name, mode) in [
        ("local", IdentityMode::Local),
        ("tailscale", IdentityMode::Tailscale),
        ("open", IdentityMode::Open),
    ] {
        assert_eq!(name.parse::<IdentityMode>(), Ok(mode));
        assert_eq!(mode.to_string(), name);
    }
    assert!(
        "Local".parse::<IdentityMode>().is_err(),
        "the names are exact",
    );
    assert!("".parse::<IdentityMode>().is_err());
}

#[tokio::test]
async fn local_mode_accepts_loopback_and_refuses_everyone_else() {
    let gate = IdentityGate::local();
    for peer in [
        "127.0.0.1:44444",
        "127.0.0.2:1",
        "[::1]:44444",
        // What a dual-stack listener reports for an IPv4 loopback peer.
        "[::ffff:127.0.0.1]:44444",
    ] {
        assert!(
            gate.authorize(addr(peer))
                .await
                .expect("loopback")
                .is_none(),
            "{peer} is this machine, and local mode resolves no identity for it",
        );
    }
    let err = gate
        .authorize(addr("100.101.102.103:44444"))
        .await
        .expect_err("a tailnet peer is not loopback");
    assert!(matches!(err, IdentityError::Forbidden(_)), "got {err:?}");
}

/// Serving a non-loopback address in `local` mode would serve it
/// unauthenticated, so the gate refuses at start-up rather than answering 403
/// forever (spec 6.11).
#[test]
fn local_mode_refuses_an_unsafe_bind() {
    let gate = IdentityGate::local();
    for address in ["0.0.0.0:6161", "100.101.102.103:6161", "[::]:6161"] {
        let err = gate
            .validate_bind(addr(address))
            .expect_err("local mode protects nothing there");
        assert!(matches!(err, IdentityError::UnsafeBind(_)), "got {err:?}");
    }
    for address in ["127.0.0.1:6161", "[::1]:6161", "127.0.0.1:0"] {
        gate.validate_bind(addr(address)).expect("loopback is fine");
    }
    // The other modes are the operator's explicit choice, so neither
    // restricts the bind.
    IdentityGate::open()
        .validate_bind(addr("0.0.0.0:6161"))
        .expect("open mode serves anything");
    IdentityGate::tailscale([], FakeWhois::failing())
        .validate_bind(addr("100.101.102.103:6161"))
        .expect("a tailnet address is the point of tailscale mode");
}

#[tokio::test]
async fn tailscale_mode_accepts_an_allowlisted_login() {
    let whois = FakeWhois::resolving(user_peer("alice@github"));
    let gate = IdentityGate::tailscale(["alice@github".to_string()], whois.resolver());

    let identity = gate
        .authorize(addr("100.101.102.103:44444"))
        .await
        .expect("an allowlisted login")
        .expect("tailscale mode resolves an identity");

    assert_eq!(identity.login.as_deref(), Some("alice@github"));
    assert_eq!(
        whois.asked(),
        vec![addr("100.101.102.103:44444")],
        "the gate looks up the peer it was handed",
    );
}

/// A tagged node has no login to allowlist, so the app capability is the only
/// way in (spec 6.11).
#[tokio::test]
async fn tailscale_mode_accepts_a_tagged_node_carrying_the_capability() {
    let whois = FakeWhois::resolving(tagged_peer(&[AJ_CONTROL_CAPABILITY]));
    let gate = IdentityGate::tailscale([], whois.resolver());

    let identity = gate
        .authorize(addr("100.64.0.9:33333"))
        .await
        .expect("the capability lets a tagged node in")
        .expect("an identity");
    assert!(identity.login.is_none(), "a tagged node has no login");
    assert_eq!(identity.tags, vec!["tag:aj-host".to_string()]);

    // The same node without the grant is refused, so the capability is doing
    // the work rather than the tag.
    let ungranted = IdentityGate::tailscale([], FakeWhois::resolving(tagged_peer(&[])));
    let err = ungranted
        .authorize(addr("100.64.0.9:33333"))
        .await
        .expect_err("no login, no capability");
    assert!(matches!(err, IdentityError::Forbidden(_)), "got {err:?}");
}

#[tokio::test]
async fn tailscale_mode_refuses_an_unlisted_login() {
    let gate = IdentityGate::tailscale(
        ["alice@github".to_string()],
        FakeWhois::resolving(user_peer("bob@github")),
    );
    let err = gate
        .authorize(addr("100.101.102.103:44444"))
        .await
        .expect_err("bob is not on the list");
    assert!(matches!(err, IdentityError::Forbidden(_)), "got {err:?}");
}

/// A lookup that cannot answer fails closed: a broken tailscaled must not
/// turn into an open control port.
#[tokio::test]
async fn tailscale_mode_fails_closed_when_the_lookup_fails() {
    let gate = IdentityGate::tailscale(["alice@github".to_string()], FakeWhois::failing());
    let err = gate
        .authorize(addr("100.101.102.103:44444"))
        .await
        .expect_err("an unresolvable peer is refused");
    assert!(matches!(err, IdentityError::Lookup(_)), "got {err:?}");

    // Including for a loopback peer: in tailscale mode nothing is exempt.
    let err = gate
        .authorize(addr("127.0.0.1:44444"))
        .await
        .expect_err("loopback is not a bypass");
    assert!(matches!(err, IdentityError::Lookup(_)), "got {err:?}");
}

#[tokio::test]
async fn open_mode_accepts_every_peer() {
    let gate = IdentityGate::open();
    for peer in ["127.0.0.1:1", "10.0.2.15:2", "[2001:db8::1]:3"] {
        assert!(
            gate.authorize(addr(peer)).await.expect("open").is_none(),
            "open mode resolves no identity",
        );
    }
}

/// The daemon's answer is much larger than what the gate needs, and it grows.
#[test]
fn the_whois_answer_is_parsed_down_to_what_the_gate_checks() {
    let raw = br#"{
        "Node": {
            "ID": 42,
            "Name": "laptop.tail-scale.ts.net.",
            "Addresses": ["100.101.102.103/32"],
            "SomethingNewer": {"nested": true}
        },
        "UserProfile": {
            "ID": 7,
            "LoginName": "alice@github",
            "DisplayName": "Alice"
        },
        "CapMap": {
            "github.com/aljoscha/aj/cap/control": [{"role": "owner"}],
            "example.com/cap/other": null
        }
    }"#;

    let identity = peer_identity_from_whois(raw).expect("the answer parses");

    assert_eq!(identity.login.as_deref(), Some("alice@github"));
    assert_eq!(identity.node, "laptop.tail-scale.ts.net.");
    assert!(identity.tags.is_empty());
    assert!(identity.capabilities.contains(AJ_CONTROL_CAPABILITY));
    assert!(identity.capabilities.contains("example.com/cap/other"));
}

/// A tagged node's whois answer carries a synthetic user profile. Treating
/// `tagged-devices` as a login would let one allowlist entry admit every
/// tagged node in the tailnet.
#[test]
fn a_tagged_node_reports_no_login() {
    let raw = br#"{
        "Node": {"Name": "aj-vm.tail-scale.ts.net.", "Tags": ["tag:aj-host"]},
        "UserProfile": {"LoginName": "tagged-devices", "DisplayName": "Tagged Devices"},
        "CapMap": {}
    }"#;

    let identity = peer_identity_from_whois(raw).expect("the answer parses");

    assert!(
        identity.login.is_none(),
        "tags are authoritative for a node"
    );
    assert_eq!(identity.tags, vec!["tag:aj-host".to_string()]);
    assert!(identity.capabilities.is_empty());
}

#[test]
fn an_unparseable_whois_answer_is_a_lookup_failure() {
    let err = peer_identity_from_whois(b"not json").expect_err("garbage does not parse");
    assert!(matches!(err, IdentityError::Lookup(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// The fixture: a real host behind a real loopback server
// ---------------------------------------------------------------------------

/// The scripted settings every session in this file starts under.
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

/// The config a test host reads, with bash's spill files aimed inside `dir`.
///
/// A background task's spill is persisted by contract, so left at the ambient
/// temp directory it would outlive the test that started the task.
fn harness_config(dir: &TempDir) -> Config {
    Config {
        spill_dir: Some(dir.path().join("spill").to_string_lossy().into_owned()),
        ..Config::default()
    }
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

/// The one row in the host's catalog: a real API, so a model change resolves
/// a provider, with credentials that resolve lazily and are never needed
/// because no turn runs after a switch to it.
///
/// The scripted bundle every session actually runs is deliberately *not* in
/// the catalog: its api has no provider registered, so a host that tried to
/// rebuild it would refuse. The catalog fallback for the host's injected
/// model is what keeps `scripted/scripted` resolvable anyway.
fn catalog_model() -> ModelInfo {
    ModelInfo {
        provider: "openai".to_string(),
        api: "openai-responses".to_string(),
        id: "gpt-catalog".to_string(),
        name: "gpt-catalog".to_string(),
        base_url: "https://catalog.example/v1".to_string(),
        ..scripted_model_info()
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

/// A turn that reads the todo list, then answers.
fn tool_turn() -> Vec<AssistantMessage> {
    vec![
        calling(
            "let me check the list",
            "call-1",
            "todo_read",
            serde_json::json!({}),
        ),
        finalized_text_message("nothing on it"),
    ]
}

/// A turn that delegates to a blocking sub-agent, then answers. Parent and
/// child share the provider, so the scripts are consumed in run order.
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

/// A turn that puts a sub-agent and a foreground tool in flight at the same
/// time: one message spawning a background sub-agent and running a bash
/// command that sleeps for `sleep_seconds`.
///
/// The two run concurrently, which is what lets a test choose which of them a
/// re-attach lands on. `report` is what the sub streams back, and its length is
/// how long the sub's run takes, one character every 20ms under
/// [`cut_provider`]. The command decides how long the parent stays blocked
/// afterwards, and while it is blocked nothing durable is appended.
fn running_tool_and_sub_turn(report: &str, sleep_seconds: u32) -> Vec<AssistantMessage> {
    let mut both = calling(
        "kicking that off",
        "call-sub",
        "agent",
        serde_json::json!({"task": "look into it", "run_in_background": true}),
    );
    both.content.push(AssistantContent::ToolCall(ToolCall {
        id: "call-slow".to_string(),
        name: "bash".to_string(),
        arguments: serde_json::json!({"command": format!("sleep {sleep_seconds}"),
                                      "description": "slow"}),
    }));
    vec![
        both,
        // Whoever asks next, which is the sub-agent.
        finalized_text_message(report),
        finalized_text_message("both of those are done"),
        // The background sub's completion notice wakes the parent, which runs
        // one more inference to acknowledge it.
        finalized_text_message("noted, thanks"),
    ]
}

/// A turn that starts a background command and keeps it running, so the task
/// table has a live row in it.
fn background_task_turn() -> Vec<AssistantMessage> {
    vec![
        calling(
            "backgrounding it",
            "call-bash",
            "bash",
            serde_json::json!({"command": "sleep 30", "run_in_background": true,
                               "description": "sleep"}),
        ),
        finalized_text_message("started it"),
    ]
}

/// A host over a temp store, served on a loopback port, plus a client for it.
struct Fixture {
    _dir: TempDir,
    host: SessionHost,
    server: RemoteServer,
    client: RemoteClient,
    /// The config the host's sessions read, and the layers a persisting
    /// settings change would write. A remote change must touch neither.
    config: Arc<StdMutex<Config>>,
    layers: Arc<StdMutex<ConfigLayers>>,
}

impl Fixture {
    async fn new(messages: Vec<AssistantMessage>) -> Self {
        Self::with_provider(scripted(messages, 0, Duration::ZERO)).await
    }

    async fn with_provider(provider: Arc<ScriptedProvider>) -> Self {
        Self::build(provider, IdentityGate::local(), Duration::from_secs(30)).await
    }

    async fn with_gate(gate: IdentityGate) -> Self {
        Self::build(
            scripted(vec![finalized_text_message("hi")], 0, Duration::ZERO),
            gate,
            Duration::from_secs(30),
        )
        .await
    }

    async fn build(
        provider: Arc<ScriptedProvider>,
        gate: IdentityGate,
        heartbeat: Duration,
    ) -> Self {
        let dir = TempDir::new().expect("tempdir");
        let config = Arc::new(StdMutex::new(harness_config(&dir)));
        let layers = Arc::new(StdMutex::new(ConfigLayers {
            user: Config::default(),
            project: ConfigLayer::default(),
            project_path: None,
        }));
        let host = SessionHost::new(HostSetup {
            config: Arc::clone(&config),
            layers: Arc::clone(&layers),
            catalog: Arc::new(vec![catalog_model()]),
            run_config: snapshot(provider),
            restore: None,
            persistence: ConversationPersistence::new(dir.path().join("sessions")),
            auth: AuthStorage::new(dir.path().join("auth.json")),
            working_directory: dir.path().to_path_buf(),
            idle_grace: None,
            live_capacity: None,
        })
        .expect("host");
        let server = RemoteServer::bind_with(host.clone(), addr("127.0.0.1:0"), gate, heartbeat)
            .await
            .expect("bind a loopback control port");
        let client = RemoteClient::new(&server.url()).expect("client");
        Self {
            _dir: dir,
            host,
            server,
            client,
            config,
            layers,
        }
    }

    /// A second client against the same server, for tests with two peers.
    fn client(&self) -> RemoteClient {
        RemoteClient::new(&self.server.url()).expect("client")
    }

    /// A second host and server over the *same* session store, for the
    /// single-writer conflict. It shares nothing else: its own config, its own
    /// catalog, its own port.
    async fn rival(&self) -> (SessionHost, RemoteServer, RemoteClient) {
        let host = SessionHost::new(HostSetup {
            config: Arc::new(StdMutex::new(harness_config(&self._dir))),
            layers: Arc::new(StdMutex::new(ConfigLayers {
                user: Config::default(),
                project: ConfigLayer::default(),
                project_path: None,
            })),
            catalog: Arc::new(vec![catalog_model()]),
            run_config: snapshot(scripted(Vec::new(), 0, Duration::ZERO)),
            restore: None,
            persistence: ConversationPersistence::new(self._dir.path().join("sessions")),
            auth: AuthStorage::new(self._dir.path().join("auth.json")),
            working_directory: self._dir.path().to_path_buf(),
            idle_grace: None,
            live_capacity: None,
        })
        .expect("a second host over the same store");
        let server = RemoteServer::bind_with(
            host.clone(),
            addr("127.0.0.1:0"),
            IdentityGate::local(),
            Duration::from_secs(30),
        )
        .await
        .expect("bind a second loopback control port");
        let client = RemoteClient::new(&server.url()).expect("client");
        (host, server, client)
    }

    /// Create a session over the wire, which is the only way a fresh host is
    /// reachable at all (spec 9.1).
    async fn create(&self) -> String {
        self.client
            .create_session(CreateSessionRequest::default())
            .await
            .expect("create a session")
            .id
    }

    async fn prompt(&self, session: &str, text: &str) {
        let outcome = self
            .client
            .command(
                session,
                &RemoteCommand::Prompt(PromptRequest {
                    agent: None,
                    input: PromptInput::Text {
                        text: text.to_string(),
                    },
                }),
            )
            .await
            .expect("prompt accepted");
        assert!(matches!(outcome, CommandOutcome::Accepted));
    }

    /// Apply one settings change over the wire.
    async fn settings(&self, session: &str, change: SessionSettings) -> Result<(), RemoteError> {
        self.client
            .command(
                session,
                &RemoteCommand::Settings(SettingsRequest {
                    agent: None,
                    change,
                }),
            )
            .await
            .map(|_| ())
    }

    /// The client folded in process: the oracle every HTTP client is compared
    /// against.
    async fn oracle(&self, session: &str) -> Attached {
        Attached::attach(Transport::Local(self.host.clone()), session).await
    }

    /// A client folded through the real HTTP stack.
    async fn remote(&self, session: &str) -> Attached {
        Attached::attach(Transport::Remote(self.client()), session).await
    }

    /// The host's whole configuration, rendered so that a change to any key
    /// shows up in a comparison.
    fn config_snapshot(&self) -> String {
        let config = self.config.lock().expect("config mutex poisoned");
        let layers = self.layers.lock().expect("layers mutex poisoned");
        format!("{config:?}|{:?}|{:?}", layers.user, layers.project)
    }

    async fn shutdown(self) {
        // Host first: an attached stream ends when the host closes it.
        self.host.shutdown().await;
        self.server.shutdown().await;
    }
}

// ---------------------------------------------------------------------------
// A transport-neutral attached client, for the equivalence harness
// ---------------------------------------------------------------------------

/// How one client reaches the host: in process, or over HTTP.
///
/// Both arms carry the attach path and the two reads a client owes after
/// `caught_up` (spec 6.5), which is what lets one fold run against either
/// transport and be compared against the other.
enum Transport {
    Local(SessionHost),
    Remote(RemoteClient),
}

impl Transport {
    async fn attach(&self, session: &str, cursor: Option<Cursor>) -> Source {
        let requests = vec![AttachRequest {
            session: session.to_string(),
            cursor,
        }];
        match self {
            Self::Local(host) => {
                let attachment = host.attach(&requests).await.expect("attach");
                assert_eq!(
                    attachment.attached(),
                    [session.to_string()],
                    "the host reports the block it will serve",
                );
                Source::Local(attachment)
            }
            Self::Remote(client) => {
                Source::Remote(client.events(&requests).await.expect("attach over http"))
            }
        }
    }

    async fn tasks(&self, session: &str) -> TaskTable {
        match self {
            Self::Local(host) => host.tasks(session).await.expect("the tasks read"),
            Self::Remote(client) => client.tasks(session).await.expect("the tasks read"),
        }
    }

    async fn queue(&self, session: &str) -> QueueState {
        match self {
            Self::Local(host) => host.queue(session).await.expect("the queue read"),
            Self::Remote(client) => client.queue(session).await.expect("the queue read"),
        }
    }

    /// Whether the host says the session's main agent has a turn in flight.
    async fn working(&self, session: &str) -> bool {
        let list = match self {
            Self::Local(host) => host.sessions().await.expect("the sessions read"),
            Self::Remote(client) => client.sessions().await.expect("the sessions read"),
        };
        list.sessions
            .iter()
            .find(|entry| entry.id == session)
            .is_some_and(|entry| entry.working)
    }
}

/// One client's frame source.
enum Source {
    Local(Attachment),
    Remote(RemoteEvents),
    /// The stream was cut and not yet replaced.
    Cut,
}

impl Source {
    async fn recv(&mut self) -> Option<Frame> {
        match self {
            Self::Local(attachment) => attachment.recv().await,
            Self::Remote(events) => match events.recv().await {
                Some(Ok(frame)) => Some(frame),
                Some(Err(err)) => panic!("the event stream failed: {err}"),
                None => None,
            },
            Self::Cut => None,
        }
    }
}

/// One attached client: the real fold, the chat model it folds into, and the
/// transport that feeds it.
struct Attached {
    transport: Transport,
    source: Source,
    client: SessionClient,
    chat: ChatState,
}

impl Attached {
    /// Attach with no cursor and apply the whole block.
    async fn attach(transport: Transport, session: &str) -> Self {
        let source = transport.attach(session, None).await;
        let mut this = Self {
            transport,
            source,
            client: SessionClient::new(session.to_string()),
            chat: ChatState::new(settings(), 200_000, Arc::new(Vec::new())),
        };
        this.client.expect_attach();
        this.apply_block().await;
        this
    }

    /// Re-attach offering the cursor this client committed.
    async fn reattach(&mut self) -> Vec<Frame> {
        let cursor = self.client.cursor();
        self.reattach_at(cursor).await
    }

    /// Re-attach offering `cursor`, whatever the fold would have offered.
    async fn reattach_at(&mut self, cursor: Option<Cursor>) -> Vec<Frame> {
        // The old stream goes first, so a client never holds two.
        self.source = Source::Cut;
        let session = self.client.session().to_string();
        self.source = self.transport.attach(&session, cursor).await;
        self.client.expect_attach();
        self.apply_block().await
    }

    /// Drop the stream without telling the host, as a lost connection does.
    fn cut(&mut self) {
        self.source = Source::Cut;
    }

    /// Apply frames through the block's `caught_up`, returning them.
    async fn apply_block(&mut self) -> Vec<Frame> {
        self.pump_until("caught_up", |frame| matches!(frame, Frame::CaughtUp { .. }))
            .await
    }

    /// Fold frames until `done` accepts one, that frame included.
    async fn pump_until(&mut self, what: &str, mut done: impl FnMut(&Frame) -> bool) -> Vec<Frame> {
        let mut seen = Vec::new();
        loop {
            let Some(frame) = bounded(what, self.source.recv()).await else {
                panic!("the stream closed before {what}");
            };
            let stop = done(&frame);
            seen.push(frame.clone());
            self.apply(frame).await;
            if stop {
                return seen;
            }
        }
    }

    /// Fold up to `count` frames, answering how many were folded.
    ///
    /// Stops early when the stream goes quiet: the exact frame count of a
    /// chunked turn varies with the host's coalescing, and a cut past the end
    /// of the turn is a valid cut rather than a reason to hang.
    async fn pump_frames(&mut self, count: usize) -> usize {
        for folded in 0..count {
            match tokio::time::timeout(QUIET, self.source.recv()).await {
                Ok(Some(frame)) => self.apply(frame).await,
                Ok(None) | Err(_) => return folded,
            }
        }
        count
    }

    /// Fold until the host reports the session idle and the stream has been
    /// quiet for [`QUIET`], so a comparison never races the tail of a turn.
    ///
    /// The host's own directory read is the authority for "idle", not the
    /// fold's `working` flag. The `state` frame that would set that flag is
    /// lossy, and one published while this client's attach block was still
    /// being written is dropped rather than queued (spec 6.5), so a client
    /// that attached just before a short turn may never be told the turn ran
    /// at all.
    async fn settle(&mut self) {
        let session = self.client.session().to_string();
        bounded("the client to settle", async {
            loop {
                match tokio::time::timeout(QUIET, self.source.recv()).await {
                    Ok(Some(frame)) => self.apply(frame).await,
                    Ok(None) => return,
                    // Quiet and idle: every frame the turn produced was
                    // published before the host stopped reporting itself
                    // working, so the gap means they have all been read.
                    Err(_) if !self.transport.working(&session).await => return,
                    // Still working: a slow turn may take its time, the outer
                    // bound is what fails a wedged one.
                    Err(_) => continue,
                }
            }
        })
        .await;
    }

    async fn apply(&mut self, frame: Frame) {
        let _ = self.client.apply(&mut self.chat, frame);
        // Neither task events nor queue updates are replayable, so every
        // `caught_up` leaves both reads outstanding (spec 6.5, 6.7). A real
        // client discharges them right there, and so does this one.
        self.discharge().await;
    }

    async fn discharge(&mut self) {
        let session = self.client.session().to_string();
        if self.client.needs_task_refetch() {
            let tasks = self.transport.tasks(&session).await;
            self.client.set_tasks(&mut self.chat, tasks);
        }
        if self.client.needs_queue_refetch() {
            let queue = self.transport.queue(&session).await;
            self.client.set_queue(&mut self.chat, queue);
        }
    }

    fn canonical(&self) -> CanonicalState {
        CanonicalState::of(&self.chat, &self.client)
    }
}

/// The two folds landed in the same place, with no dangling ids on either.
#[track_caller]
fn assert_converged(remote: &Attached, oracle: &Attached, context: &str) {
    assert_canonical_eq(&remote.canonical(), &oracle.canonical(), context);
    assert_no_dangling(&remote.chat);
    assert_no_dangling(&oracle.chat);
}

/// The transcript rows one scripted tool turn renders: the prompt, the
/// tool-calling message and its usage, the tool cell, the answer and its
/// usage.
const TURN_ROWS: usize = 6;

/// How many rows the main transcript holds, as a guard that a comparison is
/// comparing a whole turn rather than converging on something empty.
fn main_rows(state: &CanonicalState) -> usize {
    state
        .agent(AgentId::Main)
        .expect("a main transcript")
        .entries
        .len()
}

/// The tools the main transcript's cells name, in order.
fn main_tools(state: &CanonicalState) -> Vec<String> {
    state
        .agent(AgentId::Main)
        .expect("a main transcript")
        .entries
        .iter()
        .filter_map(|entry| match entry {
            CanonicalEntry::Tool { tool, .. } => Some(tool.clone()),
            _ => None,
        })
        .collect()
}

/// The text of every finalized assistant row in the main transcript, in
/// order, with each row's content blocks joined.
fn assistant_texts(state: &CanonicalState) -> Vec<String> {
    state
        .agent(AgentId::Main)
        .expect("a main transcript")
        .entries
        .iter()
        .filter_map(|entry| match entry {
            CanonicalEntry::Assistant {
                finalized: true,
                message,
                ..
            } => Some(
                message["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect()
}

/// The arguments of every still-running cell in the main transcript that names
/// `call_id`.
///
/// A list rather than a lookup, so a second cell for the same call shows up as
/// a second element instead of hiding behind the first.
fn running_cells(state: &CanonicalState, call_id: &str) -> Vec<serde_json::Value> {
    state
        .agent(AgentId::Main)
        .expect("a main transcript")
        .entries
        .iter()
        .filter_map(|entry| match entry {
            CanonicalEntry::Tool {
                call_id: id,
                status: aj_app::chat::ToolStatus::Running,
                args,
                ..
            } if id == call_id => Some(args.clone()),
            _ => None,
        })
        .collect()
}

/// The `(status, finished)` of sub-agent `child`'s box in its parent's
/// transcript.
fn sub_box(state: &CanonicalState, child: usize) -> (SubAgentStatus, bool) {
    let located = state.sub_boxes.get(&child).expect("the box is located");
    let index = located.index.expect("the box's entry resolves");
    let entry = &state
        .agent(located.agent)
        .expect("the box's transcript")
        .entries[index];
    match entry {
        CanonicalEntry::SubAgent {
            status, finished, ..
        } => (*status, *finished),
        other => panic!("expected a sub-agent box, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The handshake, creation, and the reads (spec 6.1, 6.6, 6.7)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_reports_the_protocol_and_the_working_directory() {
    let fixture = Fixture::new(Vec::new()).await;

    let hello = fixture.client.hello().await.expect("hello");

    assert_eq!(hello.protocol, PROTOCOL_VERSION);
    assert!(!hello.host_id.is_empty(), "a host names its store");
    assert_eq!(hello.app_version, env!("CARGO_PKG_VERSION"));
    assert!(
        hello.working_directory.is_some(),
        "a host serves one working directory",
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creation_applies_settings_and_runs_a_first_prompt() {
    let fixture = Fixture::new(vec![finalized_text_message("hello from the script")]).await;

    let session = fixture
        .client
        .create_session(CreateSessionRequest {
            settings: Some(SessionSettings {
                model: Some(ModelSelection {
                    api: "scripted".to_string(),
                    url: None,
                    name: "scripted".to_string(),
                }),
                speed: Some("fast".to_string()),
                ..SessionSettings::default()
            }),
            prompt: Some(PromptInput::Text {
                text: "say hello".to_string(),
            }),
            tag: None,
        })
        .await
        .expect("create with settings and a prompt")
        .id;

    let mut remote = fixture.remote(&session).await;
    remote.settle().await;

    assert_eq!(
        remote
            .client
            .settings()
            .map(|settings| settings.speed.clone()),
        Some("fast".to_string()),
        "the creator's settings are the session's settings (spec section 8)",
    );
    assert_eq!(
        assistant_texts(&remote.canonical()),
        vec!["hello from the script".to_string()],
        "the first prompt ran its turn",
    );
    let list = fixture.client.sessions().await.expect("the sessions read");
    let summary = list
        .sessions
        .iter()
        .find(|entry| entry.id == session)
        .expect("the created session is in the directory");
    assert!(summary.live, "a session this host holds is live");
    assert!(
        summary.last_seq.is_some_and(|seq| seq > 0),
        "the turn wrote log entries",
    );
    fixture.shutdown().await;
}

/// A model change travels as the (api, url, name) triple and is resolved
/// against the host's own catalog, never accepted as a catalog object
/// (spec 6.6).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_model_change_resolves_against_the_host_catalog() {
    let fixture = Fixture::new(vec![finalized_text_message("answered")]).await;
    let session = fixture.create().await;
    let mut remote = fixture.remote(&session).await;

    fixture
        .settings(
            &session,
            SessionSettings {
                model: Some(ModelSelection {
                    api: "openai".to_string(),
                    url: None,
                    name: "gpt-catalog".to_string(),
                }),
                ..SessionSettings::default()
            },
        )
        .await
        .expect("a model this host has");

    remote
        .pump_until("the refreshed state frame", |frame| {
            matches!(frame, Frame::State { settings, .. } if settings.model_id == "gpt-catalog")
        })
        .await;
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_reads_answer_tasks_queue_and_tree() {
    let fixture = Fixture::new(vec![finalized_text_message("answered")]).await;
    let session = fixture.create().await;
    let mut remote = fixture.remote(&session).await;
    fixture.prompt(&session, "hi").await;
    remote.settle().await;

    assert!(
        fixture
            .client
            .tasks(&session)
            .await
            .expect("the tasks read")
            .tasks
            .is_empty(),
        "this turn started no background task",
    );
    assert!(
        fixture
            .client
            .queue(&session)
            .await
            .expect("the queue read")
            .queues
            .is_empty(),
        "nothing is pending",
    );
    let tree = fixture.client.tree(&session).await.expect("the tree read");
    assert!(!tree.segments.is_empty(), "the log has a branch tree");

    // A withdrawal answers with the text it took, which is what makes the
    // client's dequeue gesture work (spec 6.6). Staged through the in-process
    // handles, because an idle session runs a prompt instead of queueing it.
    let handles = fixture
        .host
        .local_handles(&session)
        .await
        .expect("live session");
    handles.queues.append_follow_up(AgentId::Main, "leftover");
    let pending = fixture
        .client
        .queue(&session)
        .await
        .expect("the queue read");
    assert_eq!(pending.queues.len(), 1, "the read sees the pending message");

    let withdrawn = fixture
        .client
        .command(
            &session,
            &RemoteCommand::Queue(QueueRequest {
                op: QueueOperation::Remove,
                agent: None,
            }),
        )
        .await
        .expect("the withdrawal is accepted");
    assert!(
        matches!(&withdrawn, CommandOutcome::Withdrawn(Some(text)) if text.contains("leftover")),
        "got {withdrawn:?}",
    );
    fixture.shutdown().await;
}

/// The per-task read is what backs the task-output overlay in connect mode:
/// the host's spill file is not reachable remotely (spec 6.7).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_task_read_answers_a_live_task_and_404s_an_unknown_one() {
    let fixture = Fixture::new(background_task_turn()).await;
    let session = fixture.create().await;
    let mut remote = fixture.remote(&session).await;
    fixture.prompt(&session, "background something").await;
    remote.settle().await;

    let table = fixture
        .client
        .tasks(&session)
        .await
        .expect("the tasks read");
    let task = table
        .tasks
        .iter()
        .find(|task| task.status == aj_agent::tool::TaskStatus::Running)
        .expect("a live background task");
    let details = fixture
        .client
        .task(&session, task.id)
        .await
        .expect("the task read");
    assert_eq!(details.id, task.id);

    let err = fixture
        .client
        .task(&session, 9999)
        .await
        .expect_err("an unknown task id");
    assert_eq!(err.status(), Some(StatusCode::NOT_FOUND));
    assert_eq!(err.code(), Some("unknown_task"));

    let err = fixture
        .client
        .command(&session, &RemoteCommand::KillTask(9999))
        .await
        .expect_err("killing an unknown task");
    assert_eq!(err.status(), Some(StatusCode::NOT_FOUND));
    assert_eq!(err.code(), Some("unknown_task"));

    fixture
        .client
        .command(&session, &RemoteCommand::KillTask(task.id))
        .await
        .expect("killing a live task is accepted");

    // A path segment that is not a task id answers the protocol's error
    // shape rather than the framework's own rejection.
    let response = reqwest::Client::new()
        .get(format!(
            "{}/v1/sessions/{session}/tasks/not-a-number",
            fixture.server.url()
        ))
        .send()
        .await
        .expect("get");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: ErrorResponse = response.json().await.expect("the protocol's error shape");
    assert_eq!(body.code, "invalid_request");
    fixture.shutdown().await;
}

/// A body whose fields are all optional may be sent without one at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_with_no_body_is_accepted() {
    let fixture = Fixture::new(vec![finalized_text_message("answered")]).await;
    let session = fixture.create().await;

    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session}/cancel",
            fixture.server.url()
        ))
        .send()
        .await
        .expect("post with no body");

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    fixture.shutdown().await;
}

/// The label the host reports for `session` over the wire.
async fn remote_tag(fixture: &Fixture, session: &str) -> Option<String> {
    fixture
        .client
        .sessions()
        .await
        .expect("the sessions read")
        .sessions
        .into_iter()
        .find(|row| row.id == session)
        .expect("the session is listed")
        .tag
}

/// Post a raw tag body, so a test can send shapes the typed request cannot.
async fn post_tag(fixture: &Fixture, session: &str, body: serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session}/tag",
            fixture.server.url()
        ))
        .json(&body)
        .send()
        .await
        .expect("the request reaches the host")
}

/// The tag route sets a label and clears it, and the label reaches the row a
/// client reads back (spec 6.6, 6.8). Clearing travels as the empty string,
/// which is why there is no second route for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tag_route_sets_and_clears_a_label() {
    let fixture = Fixture::new(Vec::new()).await;
    let session = fixture.create().await;
    assert_eq!(remote_tag(&fixture, &session).await, None);

    let response = post_tag(&fixture, &session, serde_json::json!({"tag": " fix-auth "})).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(
        remote_tag(&fixture, &session).await.as_deref(),
        Some("fix-auth"),
        "the stored label is the trimmed one",
    );

    for clearing in [
        serde_json::json!({"tag": ""}),
        serde_json::json!({"tag": "   "}),
    ] {
        post_tag(&fixture, &session, serde_json::json!({"tag": "again"})).await;
        assert_eq!(
            remote_tag(&fixture, &session).await.as_deref(),
            Some("again")
        );
        let response = post_tag(&fixture, &session, clearing.clone()).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED, "{clearing}");
        assert_eq!(
            remote_tag(&fixture, &session).await,
            None,
            "{clearing} clears the label",
        );
    }

    // An unknown session is the ordinary 404, not a tag-specific answer.
    let response = post_tag(
        &fixture,
        "2020-01-01-00-00-00-000",
        serde_json::json!({"tag": "nobody"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error: ErrorResponse = response.json().await.expect("the error shape");
    assert_eq!(error.code, "unknown_session");
    fixture.shutdown().await;
}

/// A label the store would not keep is a 400 naming why, and it changes
/// nothing: the refusal happens at the wire boundary, so the session is not
/// even materialized for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_label_is_a_400_that_changes_nothing() {
    let fixture = Fixture::new(Vec::new()).await;
    let session = fixture.create().await;
    post_tag(&fixture, &session, serde_json::json!({"tag": "keep me"})).await;

    for refused in [
        serde_json::json!({"tag": "two\nlines"}),
        serde_json::json!({"tag": "bell\u{0007}"}),
        serde_json::json!({"tag": "l".repeat(aj_session::MAX_TAG_BYTES + 1)}),
    ] {
        let response = post_tag(&fixture, &session, refused.clone()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{refused}");
        let error: ErrorResponse = response.json().await.expect("the error shape");
        assert_eq!(error.code, "invalid_request", "{refused}");
        assert!(
            !error.message.is_empty(),
            "the refusal says what was wrong with {refused}",
        );
        assert_eq!(
            remote_tag(&fixture, &session).await.as_deref(),
            Some("keep me"),
            "{refused} left the label alone",
        );
    }
    fixture.shutdown().await;
}

/// A session can be created already labelled, so a client that creates and
/// lists never sees it unlabelled, and a bad label refuses the creation
/// outright rather than leaving an unlabelled session behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_is_created_with_its_label() {
    let fixture = Fixture::new(Vec::new()).await;
    let session = fixture
        .client
        .create_session(CreateSessionRequest {
            tag: Some("fix-auth".to_string()),
            ..CreateSessionRequest::default()
        })
        .await
        .expect("create a labelled session")
        .id;
    assert_eq!(
        remote_tag(&fixture, &session).await.as_deref(),
        Some("fix-auth"),
    );

    let err = fixture
        .client
        .create_session(CreateSessionRequest {
            tag: Some("two\nlines".to_string()),
            ..CreateSessionRequest::default()
        })
        .await
        .expect_err("a label the store would not keep refuses the creation");
    assert_eq!(err.status(), Some(StatusCode::BAD_REQUEST));
    assert_eq!(err.code(), Some("invalid_request"));
    assert_eq!(
        fixture
            .client
            .sessions()
            .await
            .expect("the sessions read")
            .sessions
            .len(),
        1,
        "the refused creation left no session behind",
    );
    fixture.shutdown().await;
}

/// A label the store will not write does not fail the create it was asked
/// for: the session exists, so the route answers 200 with its id and says
/// what did not land, and the client retags rather than creating a second
/// session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_label_the_store_cannot_write_still_answers_a_created_session() {
    let fixture = Fixture::new(Vec::new()).await;
    // The sidecar directory's own path, taken by a file, so no tag write
    // can land.
    let meta = fixture._dir.path().join("sessions").join("meta");
    std::fs::write(&meta, b"not a directory").expect("block the sidecar directory");

    let created = fixture
        .client
        .create_session(CreateSessionRequest {
            tag: Some("fix-auth".to_string()),
            ..CreateSessionRequest::default()
        })
        .await
        .expect("a create that minted a session is not a failed create");
    let incomplete = created
        .incomplete
        .as_deref()
        .expect("the response says the label did not land");
    assert!(
        incomplete.contains("created, tag not applied"),
        "the host's own words for what did not stick: {incomplete}",
    );
    assert!(
        fixture
            .client
            .sessions()
            .await
            .expect("the sessions read")
            .sessions
            .iter()
            .any(|entry| entry.id == created.id && entry.live),
        "the session the create minted is live and in the directory",
    );
    assert_eq!(
        remote_tag(&fixture, &created.id).await,
        None,
        "a label the store would not take is not published as if it had",
    );
    fixture.shutdown().await;
}

/// Both control arms report the same thing about a create whose label did
/// not stick: a session that exists, named by id, not a create that failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_control_arms_report_a_created_session_whose_label_did_not_stick() {
    let fixture = Fixture::new(Vec::new()).await;
    let meta = fixture._dir.path().join("sessions").join("meta");
    std::fs::write(&meta, b"not a directory").expect("block the sidecar directory");

    for control in [
        Control::local(fixture.host.clone()),
        Control::remote(fixture.client()),
    ] {
        let err = control
            .create(None, None, Some("fix-auth".to_string()))
            .await
            .expect_err("the sidecar write cannot land");
        assert!(
            !err.conflict() && !err.invalid() && !err.unknown_entry(),
            "a create that happened is none of the peer's refusals: {err}",
        );
        let ControlError::PartialCreate { session, message } = err else {
            panic!("a created session is not a refusal: {err}");
        };
        assert!(
            message.contains(&session) && message.contains("created, tag not applied"),
            "the message names the session that exists: {message}",
        );
        assert!(
            fixture
                .client
                .sessions()
                .await
                .expect("the sessions read")
                .sessions
                .iter()
                .any(|entry| entry.id == session && entry.live),
            "the session {session} the create minted is live and in the directory",
        );
    }
    fixture.shutdown().await;
}

// ---------------------------------------------------------------------------
// The status vocabulary (spec 6.1)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_session_answers_404_on_every_route() {
    let fixture = Fixture::new(Vec::new()).await;
    let missing = "no-such-session";

    let mut refusals = vec![
        fixture.client.tasks(missing).await.err(),
        fixture.client.queue(missing).await.err(),
        fixture.client.tree(missing).await.err(),
        fixture.client.task(missing, 1).await.err(),
    ];
    for command in [
        RemoteCommand::Prompt(PromptRequest {
            agent: None,
            input: PromptInput::Text {
                text: "hi".to_string(),
            },
        }),
        RemoteCommand::Steer(SteerRequest {
            text: "hi".to_string(),
            agent: None,
        }),
        RemoteCommand::Cancel(CancelRequest::default()),
        RemoteCommand::Queue(QueueRequest {
            op: QueueOperation::Clear,
            agent: None,
        }),
        RemoteCommand::Compact(CompactRequest::default()),
        RemoteCommand::Settings(SettingsRequest {
            agent: None,
            change: SessionSettings {
                thinking: Some("off".to_string()),
                ..SessionSettings::default()
            },
        }),
        RemoteCommand::Head(HeadRequest::entry("whatever".to_string())),
        RemoteCommand::KillTask(1),
    ] {
        refusals.push(fixture.client.command(missing, &command).await.err());
    }
    // Attaching an unknown session fails as a status before the stream opens.
    refusals.push(
        fixture
            .client
            .events(&[AttachRequest {
                session: missing.to_string(),
                cursor: None,
            }])
            .await
            .err(),
    );

    assert_eq!(refusals.len(), 13, "every session-scoped route is covered");
    for refusal in refusals {
        let err = refusal.expect("an unknown session is refused");
        assert_eq!(err.status(), Some(StatusCode::NOT_FOUND), "got {err}");
        assert_eq!(err.code(), Some("unknown_session"), "got {err}");
    }
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_traversal_id_is_refused_at_the_wire_boundary() {
    let fixture = Fixture::new(Vec::new()).await;
    // A readable log just outside the store, so a resolved traversal would
    // find something. An empty log counts as the current format.
    let outside = fixture._dir.path().join("elsewhere");
    std::fs::create_dir_all(&outside).expect("a directory beside the store");
    std::fs::write(outside.join("reachable.jsonl"), "").expect("a log outside the store");

    // Ids that reach a handler as a path segment. The percent-encoded one is
    // the interesting case: the framework decodes it back into `../` before
    // the handler sees it.
    for id in ["..%2Felsewhere%2Freachable", "sneaky.jsonl", "host-id."] {
        let err = fixture
            .client
            .tasks(id)
            .await
            .err()
            .unwrap_or_else(|| panic!("{id:?} was served"));
        assert_eq!(err.status(), Some(StatusCode::NOT_FOUND), "{id:?}: {err}");
        assert_eq!(err.code(), Some("unknown_session"), "{id:?}: {err}");
    }

    // An id with literal separators never becomes a session route at all: the
    // client's own URL parser resolves the `..` out of the path, so it lands
    // on an endpoint that does not exist. Also 404, and it reaches no store.
    let err = fixture
        .client
        .tasks("../elsewhere/reachable")
        .await
        .err()
        .expect("a traversal path is served by nothing");
    assert_eq!(err.status(), Some(StatusCode::NOT_FOUND), "got {err}");

    // The stream route carries its id in a query parameter, where nothing
    // normalizes it, so this is the one that really exercises the host's
    // gate. An empty id answers the same way.
    for id in ["../elsewhere/reachable", "..", ""] {
        let err = fixture
            .client
            .events(&[AttachRequest {
                session: id.to_string(),
                cursor: None,
            }])
            .await
            .err()
            .unwrap_or_else(|| panic!("{id:?} opened a stream"));
        assert_eq!(err.status(), Some(StatusCode::NOT_FOUND), "{id:?}: {err}");
        assert_eq!(err.code(), Some("unknown_session"), "{id:?}: {err}");
    }

    assert!(
        outside.join("reachable.jsonl").is_file(),
        "the traversal target is still there, untouched",
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_settings_body_answers_400() {
    let fixture = Fixture::new(Vec::new()).await;
    let session = fixture.create().await;

    let bodies = [
        // No axis at all.
        SessionSettings::default(),
        // Two axes: the host applies, logs and publishes one at a time.
        SessionSettings {
            thinking: Some("off".to_string()),
            speed: Some("fast".to_string()),
            ..SessionSettings::default()
        },
        // Names outside the vocabulary, one axis at a time.
        SessionSettings {
            thinking: Some("ludicrous".to_string()),
            ..SessionSettings::default()
        },
        SessionSettings {
            speed: Some("warp".to_string()),
            ..SessionSettings::default()
        },
        SessionSettings {
            thinking_display: Some("interpretive-dance".to_string()),
            ..SessionSettings::default()
        },
        SessionSettings {
            verbosity: Some("shouty".to_string()),
            ..SessionSettings::default()
        },
        // An incomplete model triple is malformed rather than unservable.
        SessionSettings {
            model: Some(ModelSelection {
                api: String::new(),
                url: None,
                name: "gpt-catalog".to_string(),
            }),
            ..SessionSettings::default()
        },
    ];
    for change in bodies {
        let err = fixture
            .settings(&session, change.clone())
            .await
            .expect_err(&format!("{change:?} is malformed"));
        assert_eq!(err.status(), Some(StatusCode::BAD_REQUEST), "got {err}");
        assert_eq!(err.code(), Some("invalid_request"), "got {err}");
    }

    // A body that is not JSON at all gets the same shape.
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session}/settings",
            fixture.server.url()
        ))
        .body("{not json")
        .send()
        .await
        .expect("post");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: ErrorResponse = response.json().await.expect("the protocol's error shape");
    assert_eq!(body.code, "invalid_request");
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_head_switch_is_refused_with_409_while_a_turn_runs() {
    // A slow-streaming turn, so the switch lands mid-turn.
    let fixture = Fixture::with_provider(scripted(
        vec![finalized_text_message("a fairly long answer to stream")],
        1,
        Duration::from_millis(20),
    ))
    .await;
    let session = fixture.create().await;
    let mut remote = fixture.remote(&session).await;
    fixture.prompt(&session, "hi").await;

    let err = fixture
        .client
        .command(
            &session,
            &RemoteCommand::Head(HeadRequest::entry("whatever".to_string())),
        )
        .await
        .expect_err("a mid-turn head switch is refused");
    assert_eq!(err.status(), Some(StatusCode::CONFLICT));
    assert_eq!(err.code(), Some("conflict"));

    remote.settle().await;
    // Idle again, and now the entry id is what is wrong: a 404, not a 409.
    let err = fixture
        .client
        .command(
            &session,
            &RemoteCommand::Head(HeadRequest::entry("no-such-entry".to_string())),
        )
        .await
        .expect_err("an unknown entry is refused");
    assert_eq!(err.status(), Some(StatusCode::NOT_FOUND));
    assert_eq!(err.code(), Some("unknown_entry"));
    fixture.shutdown().await;
}

/// A head switch names exactly one target. Both fields optional is what lets
/// the two shapes share one body, so the rule is the wire boundary's to
/// enforce, and a blank body decodes to a request naming neither.
#[tokio::test]
async fn a_head_switch_naming_no_target_or_two_answers_400() {
    let fixture = Fixture::new(Vec::new()).await;
    let session = fixture.create().await;

    for body in [
        serde_json::json!({}),
        serde_json::json!({"entry": "entry-1", "before": "entry-2"}),
        // A blank body, which the extractor reads as `{}`.
        serde_json::json!(null),
    ] {
        let mut request = reqwest::Client::new().post(format!(
            "{}/v1/sessions/{session}/head",
            fixture.server.url()
        ));
        if !body.is_null() {
            request = request.json(&body);
        }
        let response = request.send().await.expect("the request reaches the host");
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{body} names no single target",
        );
        let error: ErrorResponse = response.json().await.expect("the error shape");
        assert_eq!(error.code, "invalid_request", "{body}");
    }
    fixture.shutdown().await;
}

/// The `before` shape crosses HTTP and lands on the named entry's parent,
/// which is what makes a branch replace the message it was taken from (spec
/// 6.6). An unknown entry is a 404 and the session's first entry is refused,
/// so a client cannot branch a session into having no history.
#[tokio::test]
async fn a_head_switch_before_an_entry_resolves_over_http() {
    let fixture = Fixture::new(vec![finalized_text_message("answered")]).await;
    let session = fixture.create().await;
    let mut remote = fixture.remote(&session).await;
    fixture.prompt(&session, "hi").await;
    remote.settle().await;

    let (user_message, its_parent, root) = {
        let handles = fixture
            .host
            .local_handles(&session)
            .await
            .expect("the host holds the session live");
        let log = handles.log.lock().await;
        let entries = log.entries_in_order();
        let user = entries
            .iter()
            .find(|entry| {
                matches!(
                    &entry.entry,
                    aj_session::ConversationEntryKind::Message { message }
                        if matches!(
                            message.as_stored_wire(),
                            Some(aj_models::types::Message::User(_))
                        )
                )
            })
            .expect("the prompt was logged");
        (
            user.id.clone(),
            user.parent_id.clone().expect("a user message has a parent"),
            entries.first().expect("a first entry").id.clone(),
        )
    };

    fixture
        .client
        .command(
            &session,
            &RemoteCommand::Head(HeadRequest::before(user_message.clone())),
        )
        .await
        .expect("branching before a logged message is accepted");
    assert_eq!(
        fixture
            .client
            .tree(&session)
            .await
            .expect("the tree read")
            .head,
        Some(its_parent),
        "the head landed on the message's parent, not on the message",
    );

    let err = fixture
        .client
        .command(
            &session,
            &RemoteCommand::Head(HeadRequest::before("no-such-entry".to_string())),
        )
        .await
        .expect_err("an unknown entry is refused");
    assert_eq!(err.status(), Some(StatusCode::NOT_FOUND));
    assert_eq!(err.code(), Some("unknown_entry"));

    let err = fixture
        .client
        .command(&session, &RemoteCommand::Head(HeadRequest::before(root)))
        .await
        .expect_err("there is nothing before the first entry");
    assert_eq!(err.status(), Some(StatusCode::BAD_REQUEST));
    assert_eq!(err.code(), Some("invalid_request"));
    fixture.shutdown().await;
}

/// A session another host holds is a 409 `locked`: materializing takes the
/// session's advisory lock, and a second writer on one log would corrupt it
/// (spec section 5). Every route that would materialize answers it, and the
/// reads that do not materialize answer for a cold session instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_another_host_holds_answers_409_locked() {
    let fixture = Fixture::new(vec![
        finalized_text_message("answered"),
        finalized_text_message("still mine"),
    ])
    .await;
    let session = fixture.create().await;
    let mut remote = fixture.remote(&session).await;
    // A turn first, so the log exists on disk and the store scan finds it: the
    // log is created lazily and a session with nothing in it is not discoverable.
    fixture.prompt(&session, "hi").await;
    remote.settle().await;
    let (rival_host, rival_server, rival) = fixture.rival().await;

    // The session is on disk, so the rival's directory lists it. What it
    // cannot do is take it over.
    assert!(
        rival
            .sessions()
            .await
            .expect("the sessions read")
            .sessions
            .iter()
            .any(|entry| entry.id == session),
        "the rival shares the store, so it discovers the session",
    );

    let mut refusals = vec![
        rival
            .events(&[AttachRequest {
                session: session.clone(),
                cursor: None,
            }])
            .await
            .err(),
        rival
            .command(
                &session,
                &RemoteCommand::Prompt(PromptRequest {
                    agent: None,
                    input: PromptInput::Text {
                        text: "mine now".to_string(),
                    },
                }),
            )
            .await
            .err(),
        rival.tree(&session).await.err(),
    ];
    assert_eq!(refusals.len(), 3, "every materializing route is covered");
    for refusal in refusals.drain(..) {
        let err = refusal.expect("the lock refuses");
        assert_eq!(err.status(), Some(StatusCode::CONFLICT), "got {err}");
        assert_eq!(err.code(), Some("locked"), "got {err}");
    }

    // The reads that do not materialize answer instead, for a session that is
    // cold as far as this host is concerned (spec 6.7). A lock refusal here
    // would be wrong: nothing about them takes the log.
    assert!(
        rival
            .tasks(&session)
            .await
            .expect("the tasks read answers")
            .tasks
            .is_empty(),
        "a session this host does not hold has no tasks",
    );
    assert!(
        rival
            .queue(&session)
            .await
            .expect("the queue read answers")
            .queues
            .is_empty(),
        "and nothing queued",
    );
    let unknown = rival
        .task(&session, 1)
        .await
        .expect_err("and no task to read");
    assert_eq!(
        unknown.status(),
        Some(StatusCode::NOT_FOUND),
        "got {unknown}"
    );
    assert_eq!(unknown.code(), Some("unknown_task"), "got {unknown}");

    // And the first host still has it: the refused materialization touched
    // neither the log nor the lock.
    fixture.prompt(&session, "again").await;
    remote.settle().await;
    assert_eq!(
        assistant_texts(&remote.canonical()),
        vec!["answered".to_string(), "still mine".to_string()],
        "the holder still drives the session",
    );

    rival_host.shutdown().await;
    rival_server.shutdown().await;
    fixture.shutdown().await;
}

/// A model the host cannot serve is a 409, not a 400: nothing about the
/// request is malformed and another host may well serve it (spec 6.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_the_host_does_not_have_answers_409() {
    let fixture = Fixture::new(Vec::new()).await;
    let session = fixture.create().await;

    let err = fixture
        .settings(
            &session,
            SessionSettings {
                model: Some(ModelSelection {
                    api: "anthropic".to_string(),
                    url: None,
                    name: "claude-not-in-this-catalog".to_string(),
                }),
                ..SessionSettings::default()
            },
        )
        .await
        .expect_err("the host has no such model");
    assert_eq!(err.status(), Some(StatusCode::CONFLICT));
    assert_eq!(err.code(), Some("unsupported"));
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_endpoint_answers_404() {
    let fixture = Fixture::new(Vec::new()).await;

    let response = reqwest::Client::new()
        .get(format!("{}/v1/nothing-here", fixture.server.url()))
        .send()
        .await
        .expect("a request to a route that does not exist");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: ErrorResponse = response.json().await.expect("the protocol's error shape");
    assert_eq!(body.code, "unknown_endpoint");
    fixture.shutdown().await;
}

/// A settings change from the network is session-only however it was asked
/// for: a peer must not be able to rewrite the host's config files.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_settings_change_never_persists() {
    let fixture = Fixture::new(vec![finalized_text_message("answered")]).await;
    let session = fixture.create().await;
    let mut remote = fixture.remote(&session).await;
    let before = fixture.config_snapshot();

    fixture
        .settings(
            &session,
            SessionSettings {
                thinking: Some("high".to_string()),
                ..SessionSettings::default()
            },
        )
        .await
        .expect("the change applies");
    remote
        .pump_until(
            "the refreshed state frame",
            |frame| matches!(frame, Frame::State { settings, .. } if settings.thinking == "high"),
        )
        .await;

    // A persisting change mutates the user layer and the effective config in
    // memory before it writes a file, so an untouched pair is evidence that
    // nothing was written.
    assert_eq!(
        fixture.config_snapshot(),
        before,
        "a remote change touched the host's configuration",
    );
    fixture.shutdown().await;
}

// ---------------------------------------------------------------------------
// The stream (spec 6.1, 6.3, 6.5)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_drives_a_turn_observed_on_the_stream() {
    let fixture = Fixture::new(tool_turn()).await;
    let session = fixture.create().await;
    let mut remote = fixture.remote(&session).await;

    fixture.prompt(&session, "check the todos").await;
    remote.settle().await;

    let state = remote.canonical();
    assert_eq!(main_rows(&state), TURN_ROWS, "the whole turn: {state:?}");
    assert_eq!(
        main_tools(&state),
        vec!["todo_read".to_string()],
        "the tool cell is there",
    );
    assert!(
        !remote.client.working(),
        "the host reported the turn finished",
    );
    assert!(
        remote.client.cursor().is_some(),
        "durable frames advanced the cursor",
    );
    fixture.shutdown().await;
}

/// One stream carries every session it names, each with its own block.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_stream_attaches_several_sessions() {
    let fixture = Fixture::new(Vec::new()).await;
    let first = fixture.create().await;
    let second = fixture.create().await;

    let mut events = fixture
        .client
        .events(&[
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
        .expect("attach two sessions");

    let mut blocks = Vec::new();
    bounded("both attach blocks", async {
        while blocks.len() < 2 {
            match events.recv().await {
                Some(Ok(Frame::CaughtUp { session, .. })) => blocks.push(session),
                Some(Ok(_)) => {}
                Some(Err(err)) => panic!("the stream failed: {err}"),
                None => panic!("the stream closed early"),
            }
        }
    })
    .await;
    blocks.sort();
    let mut expected = vec![first, second];
    expected.sort();
    assert_eq!(blocks, expected, "one block per named session");
    fixture.shutdown().await;
}

/// An idle stream heartbeats with a real frame, and the timer restarts after
/// every write, so a client can tell a live connection from a stalled one
/// (spec 6.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_stream_heartbeats_with_a_real_frame() {
    let fixture = Fixture::build(
        scripted(Vec::new(), 0, Duration::ZERO),
        IdentityGate::local(),
        Duration::from_millis(80),
    )
    .await;
    let session = fixture.create().await;
    let mut events = fixture
        .client
        .events(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");

    let mut heartbeats = 0;
    bounded("two heartbeats", async {
        while heartbeats < 2 {
            match events.recv().await {
                Some(Ok(Frame::Heartbeat)) => heartbeats += 1,
                Some(Ok(_)) => {}
                Some(Err(err)) => panic!("the stream failed: {err}"),
                None => panic!("the stream closed early"),
            }
        }
    })
    .await;
    assert_eq!(heartbeats, 2, "the idle timer restarts after each write");
    fixture.shutdown().await;
}

/// Silence is what a dead stream looks like to a client, and it has to end
/// with an error the caller can reconnect from (spec 6.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silent_stream_is_reported_dead() {
    // A heartbeat interval far beyond the client's tolerance, so the stream
    // is alive and silent, which is exactly the case under test.
    let fixture = Fixture::build(
        scripted(Vec::new(), 0, Duration::ZERO),
        IdentityGate::local(),
        Duration::from_secs(60),
    )
    .await;
    let session = fixture.create().await;
    let client = fixture.client().with_silence(Duration::from_millis(200));
    let mut events = client
        .events(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");

    // The host still has a `list` frame to publish on its coalescing tick,
    // so the silence is only reached once everything it owed has been read.
    let err = bounded("the silence to be noticed", async {
        loop {
            match events.recv().await {
                Some(Ok(_)) => continue,
                Some(Err(err)) => return err,
                None => panic!("the stream ended instead of going silent"),
            }
        }
    })
    .await;

    assert!(matches!(err, RemoteError::Silent(_)), "got {err:?}");
    assert!(
        bounded("the stream to end", events.recv()).await.is_none(),
        "a dead stream stays dead",
    );
    fixture.shutdown().await;
}

/// The silence deadline belongs to the stream, not to a call on it.
///
/// The drive loop never parks on the stream alone: it drains with `try_recv`
/// every iteration and re-creates its awaiting `recv` future whenever another
/// `select!` arm wins. A deadline measured from the call would restart on
/// every one of those, and a host wedged mid-turn (whose spinner keeps the
/// loop iterating) would never be declared dead (spec 6.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silent_stream_is_reported_dead_to_a_polling_client() {
    let silence = Duration::from_millis(300);
    // A heartbeat interval far beyond the client's tolerance, so the stream is
    // alive and silent, which is exactly the case under test.
    let fixture = Fixture::build(
        scripted(Vec::new(), 0, Duration::ZERO),
        IdentityGate::local(),
        Duration::from_secs(60),
    )
    .await;
    let session = fixture.create().await;
    let control = Control::remote(fixture.client().with_silence(silence));
    let mut stream = control
        .attach_all(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("the attach is served");

    // The loop's shape: drain what is ready, then go do something else. Held
    // for well past the tolerance, so a deadline the drain restarts leaves the
    // stream looking alive forever.
    let polling_until = std::time::Instant::now() + silence * 3;
    while std::time::Instant::now() < polling_until {
        while stream.try_recv().is_some() {}
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // `try_recv` holds the failure back for `recv`, which is the one place the
    // loop reacts to a lost stream. It has to be there already: a `recv` that
    // has to wait out a silence window of its own is a deadline the drain
    // above kept resetting.
    let waited = std::time::Instant::now();
    let frame = bounded("the silence to be reported", stream.recv()).await;
    let waited = waited.elapsed();
    let ControlFrame::Lost(err) = frame else {
        panic!("a stream silent for {silence:?} is still reported live");
    };
    assert!(
        err.to_string().contains("silent"),
        "the loss names the silence: {err}",
    );
    assert!(
        waited < silence,
        "the loss was noticed only by this call, after {waited:?} of its own",
    );
    fixture.shutdown().await;
}

/// Opening the stream is bounded even though the body it opens is not: a host
/// that accepts the connection and never answers must not park the caller,
/// which for the TUI's reconnect path would freeze input and redraw with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opening_a_stream_against_a_mute_host_is_abandoned() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let addr = listener.local_addr().expect("the bound address");
    // Accepts and holds: the connection is established, the response head
    // never comes.
    let mute = tokio::spawn(async move {
        let mut accepted = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            accepted.push(socket);
        }
    });

    let client = RemoteClient::new(&format!("http://{addr}"))
        .expect("client")
        .with_open_timeout(Duration::from_millis(200));
    let Err(err) = bounded(
        "the open to be abandoned",
        client.events(&[AttachRequest {
            session: "whatever".to_string(),
            cursor: None,
        }]),
    )
    .await
    else {
        panic!("a mute host answered a stream request");
    };

    assert!(
        matches!(&err, RemoteError::Stream(reason) if reason.contains("did not answer")),
        "got {err:?}",
    );
    mute.abort();
}

// ---------------------------------------------------------------------------
// Client decode rules the real host cannot produce (spec 6.10)
// ---------------------------------------------------------------------------

/// A stand-in server that answers canned bodies: a frame kind this build
/// does not know, a malformed known frame, a protocol from the future.
async fn canned_server(
    hello: serde_json::Value,
    frames: Vec<String>,
) -> (String, tokio::task::JoinHandle<()>) {
    use axum::response::sse::{Event, Sse};
    use axum::routing::get;

    let app = axum::Router::new()
        .route(
            "/v1/hello",
            get(move || {
                let hello = hello.clone();
                async move { axum::Json(hello) }
            }),
        )
        .route(
            "/v1/events",
            get(move || {
                let frames = frames.clone();
                async move {
                    Sse::new(futures::stream::iter(frames.into_iter().map(|data| {
                        Ok::<_, std::convert::Infallible>(Event::default().data(data))
                    })))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind(addr("127.0.0.1:0"))
        .await
        .expect("bind");
    let bound = listener.local_addr().expect("local addr");
    let serving = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{bound}"), serving)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_protocol_this_build_does_not_speak_fails_the_handshake() {
    let (url, serving) = canned_server(
        serde_json::json!({
            "protocol": PROTOCOL_VERSION + 1,
            "capabilities": ["something-new"],
            "app_version": "9.9.9",
            "host_id": "future",
        }),
        Vec::new(),
    )
    .await;

    let err = RemoteClient::new(&url)
        .expect("client")
        .hello()
        .await
        .expect_err("a newer protocol is not spoken");

    assert!(
        matches!(err, RemoteError::Protocol { found, expected }
            if found == PROTOCOL_VERSION + 1 && expected == PROTOCOL_VERSION),
        "got {err:?}",
    );
    serving.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_frame_kind_is_skipped_and_a_malformed_known_one_errors() {
    let (url, serving) = canned_server(
        serde_json::json!({"protocol": PROTOCOL_VERSION, "capabilities": [],
                           "app_version": "0", "host_id": "canned"}),
        vec![
            // A kind from a newer peer: discarded by an endpoint client.
            r#"{"kind":"something_newer","session":"s","payload":{"a":1}}"#.to_string(),
            r#"{"kind":"heartbeat"}"#.to_string(),
            // A known kind missing required fields: an error, not a downgrade.
            r#"{"kind":"caught_up","session":"s"}"#.to_string(),
        ],
    )
    .await;
    let client = RemoteClient::new(&url).expect("client");
    let mut events = client.events(&[]).await.expect("open the canned stream");

    assert!(
        matches!(
            bounded("the heartbeat", events.recv()).await,
            Some(Ok(Frame::Heartbeat))
        ),
        "the unknown kind was skipped and the next frame delivered",
    );
    let err = bounded("the malformed frame", events.recv())
        .await
        .expect("an item")
        .expect_err("a malformed known frame is an error");
    assert!(matches!(err, RemoteError::Decode(_)), "got {err:?}");
    assert!(
        bounded("the end of the stream", events.recv())
            .await
            .is_none(),
        "a failed stream ends",
    );
    serving.abort();
}

/// The same stream read as decoded frames keeps the unknown kind, with its
/// JSON intact. That is the form a gateway forwards from (spec 6.10), so the
/// discarding an endpoint client does must not be the only way to read a
/// stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_decoded_stream_keeps_an_unknown_frame_whole() {
    let (url, serving) = canned_server(
        serde_json::json!({"protocol": PROTOCOL_VERSION, "capabilities": [],
                           "app_version": "0", "host_id": "canned"}),
        vec![
            r#"{"kind":"something_newer","session":"s","payload":{"a":1}}"#.to_string(),
            r#"{"kind":"heartbeat"}"#.to_string(),
            r#"{"kind":"caught_up","session":"s"}"#.to_string(),
        ],
    )
    .await;
    let client = RemoteClient::new(&url).expect("client");
    let mut events = client.events(&[]).await.expect("open the canned stream");

    let frame = bounded("the unknown frame", events.recv_decoded())
        .await
        .expect("an item")
        .expect("an unknown kind is not an error");
    let DecodedFrame::Unknown { kind, raw } = frame else {
        panic!("the unknown kind decoded as known");
    };
    assert_eq!(kind, "something_newer");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(raw.get()).expect("the retained JSON"),
        serde_json::json!({"kind":"something_newer","session":"s","payload":{"a":1}}),
        "the whole frame is retained, not just the fields this build knows",
    );

    let frame = bounded("the heartbeat", events.recv_decoded())
        .await
        .expect("an item")
        .expect("a known kind");
    assert!(
        matches!(frame, DecodedFrame::Known(known) if matches!(known.value(), Frame::Heartbeat)),
        "a known kind still decodes",
    );
    // A malformed known frame is an error on this form too: it is the same
    // reliable frame nobody can apply.
    let err = bounded("the malformed frame", events.recv_decoded())
        .await
        .expect("an item")
        .expect_err("a malformed known frame is an error");
    assert!(matches!(err, RemoteError::Decode(_)), "got {err:?}");
    assert!(
        bounded("the end of the stream", events.recv_decoded())
            .await
            .is_none(),
        "a failed stream ends",
    );
    serving.abort();
}

#[test]
fn a_base_url_has_to_be_absolute_http() {
    for base in ["", "not a url", "ftp://host", "host:6161", "/v1"] {
        assert!(
            matches!(RemoteClient::new(base), Err(RemoteError::InvalidUrl { .. })),
            "{base:?} is not a base url",
        );
    }
    let client = RemoteClient::new("http://127.0.0.1:6161/").expect("a base url");
    assert_eq!(
        client.base(),
        "http://127.0.0.1:6161",
        "the trailing slash goes, so routes concatenate cleanly",
    );
}

// ---------------------------------------------------------------------------
// The identity gate over HTTP (spec 6.11, 11.4)
// ---------------------------------------------------------------------------

/// Every route, so a gate that let one through by accident is caught.
async fn probe_every_route(client: &RemoteClient, session: &str) -> Vec<Result<(), RemoteError>> {
    let commands = [
        RemoteCommand::Prompt(PromptRequest {
            agent: None,
            input: PromptInput::Text {
                text: "hi".to_string(),
            },
        }),
        RemoteCommand::Steer(SteerRequest {
            text: "hi".to_string(),
            agent: None,
        }),
        RemoteCommand::Cancel(CancelRequest::default()),
        RemoteCommand::Queue(QueueRequest {
            op: QueueOperation::Clear,
            agent: None,
        }),
        RemoteCommand::Compact(CompactRequest::default()),
        RemoteCommand::Settings(SettingsRequest {
            agent: None,
            change: SessionSettings {
                thinking: Some("off".to_string()),
                ..SessionSettings::default()
            },
        }),
        RemoteCommand::Tag(TagRequest {
            tag: "probe".to_string(),
        }),
        RemoteCommand::Head(HeadRequest::entry("whatever".to_string())),
        RemoteCommand::KillTask(1),
    ];

    let mut probes = vec![
        client.hello().await.map(|_| ()),
        client.sessions().await.map(|_| ()),
        client
            .create_session(CreateSessionRequest::default())
            .await
            .map(|_| ()),
        client.tasks(session).await.map(|_| ()),
        client.task(session, 1).await.map(|_| ()),
        client.queue(session).await.map(|_| ()),
        client.tree(session).await.map(|_| ()),
        client
            .events(&[AttachRequest {
                session: session.to_string(),
                cursor: None,
            }])
            .await
            .map(|_| ()),
    ];
    for command in &commands {
        probes.push(client.command(session, command).await.map(|_| ()));
    }
    probes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_peer_gets_403_on_every_route() {
    let whois = FakeWhois::resolving(user_peer("intruder@github"));
    let fixture = Fixture::with_gate(IdentityGate::tailscale(
        ["alice@github".to_string()],
        whois.resolver(),
    ))
    .await;
    // Created behind the gate's back, so a 403 cannot be mistaken for a 404.
    let session = fixture.host.create().await.expect("create a session");

    let probes = probe_every_route(&fixture.client, &session).await;

    assert_eq!(probes.len(), 17, "every route is probed");
    for probe in probes {
        let err = probe.expect_err("the gate refuses");
        assert_eq!(err.status(), Some(StatusCode::FORBIDDEN), "got {err}");
        assert_eq!(err.code(), Some("forbidden"), "got {err}");
        assert!(
            !err.to_string().contains("alice@github"),
            "a refusal does not hand the allowlist to the peer: {err}",
        );
    }
    assert!(
        !whois.asked().is_empty(),
        "the gate resolved the connection's peer",
    );
    assert!(
        whois.asked().iter().all(|peer| peer.ip().is_loopback()),
        "and it was handed the connection's real address: {:?}",
        whois.asked(),
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authorized_peer_reaches_every_route() {
    let fixture = Fixture::with_gate(IdentityGate::tailscale(
        [],
        FakeWhois::resolving(tagged_peer(&[AJ_CONTROL_CAPABILITY])),
    ))
    .await;
    let session = fixture.create().await;

    let probes = probe_every_route(&fixture.client, &session).await;

    for (index, probe) in probes.into_iter().enumerate() {
        // Three of the probes name something that does not exist (task 1,
        // and the head entry), so they are refused on their own merits. What
        // matters here is that nothing is refused by the gate.
        if let Err(err) = probe {
            assert_eq!(
                err.status(),
                Some(StatusCode::NOT_FOUND),
                "probe {index} was refused by something other than its own merits: {err}",
            );
        }
    }
    fixture.shutdown().await;
}

// ---------------------------------------------------------------------------
// Reducer equivalence (spec 11.2)
// ---------------------------------------------------------------------------

/// The core property: a client fed through the real HTTP stack lands on the
/// same state as one attached in process to the same host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_http_client_converges_with_an_in_process_oracle() {
    for script in [tool_turn(), sub_agent_turn()] {
        let fixture = Fixture::new(script).await;
        let session = fixture.create().await;
        let mut oracle = fixture.oracle(&session).await;
        let mut remote = fixture.remote(&session).await;

        fixture.prompt(&session, "do the thing").await;
        oracle.settle().await;
        remote.settle().await;

        assert_eq!(
            main_rows(&oracle.canonical()),
            TURN_ROWS,
            "the compared state is a whole turn",
        );
        assert_converged(&remote, &oracle, "a whole turn over http");
        fixture.shutdown().await;
    }
}

/// The task table is not replayable, so a client owes the tasks read after
/// every `caught_up` (spec 6.5, 6.7). A joiner that arrives after the task
/// started has to end up with the table a client that watched it start has,
/// and with the same launch cell: badge, structured body and wire content.
///
/// The scripted task is quiet, which is what makes the whole canonical state
/// comparable here. A task that streamed output before the joiner attached
/// would leave the two apart on that cell's structured body: `TaskOutput` is
/// lossy, the newest snapshot is the only carrier of a task's rolling output,
/// and nothing re-sends it to a client that was not connected for it.
///
/// The `details` comparison rides on that quiet a second way. Two events write
/// the launch cell's structured body, the launch's own `ToolExecutionEnd` and
/// the task driver's first `TaskOutput`, and `TaskStart` may land on either
/// side of the launch result (the bash tool documents that race at its
/// registration site). Whichever order they arrive in, the two clients agree
/// because the driver's leading-edge snapshot of a task with no output yet is
/// byte-identical to the launch result, `task_id` and `full_output_path`
/// included. Whoever changes what that snapshot carries breaks this test, and
/// the fix is here rather than in the ordering.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiner_refetches_the_task_table_after_caught_up() {
    let fixture = Fixture::new(background_task_turn()).await;
    let session = fixture.create().await;
    let mut oracle = fixture.oracle(&session).await;
    fixture.prompt(&session, "background something").await;
    oracle.settle().await;
    assert_eq!(
        oracle.canonical().tasks.len(),
        1,
        "the oracle watched the task start",
    );

    let mut joiner = fixture.remote(&session).await;
    joiner.settle().await;

    assert_eq!(
        joiner.chat.tasks().len(),
        1,
        "the joiner's table came from the read: a backfill carries no task events",
    );
    let launch = joiner
        .canonical()
        .agent(AgentId::Main)
        .expect("a main transcript")
        .entries
        .iter()
        .find_map(|entry| match entry {
            CanonicalEntry::Tool { task, details, .. } => Some((*task, details.clone())),
            _ => None,
        })
        .expect("the launch cell");
    assert_eq!(
        launch.0,
        Some(1),
        "the tasks read badged the joiner's launch cell",
    );
    assert_eq!(
        launch.1.as_ref().map(|details| &details["task_id"]),
        Some(&serde_json::json!(1)),
        "and the persisted details name the task too",
    );
    assert_canonical_eq(
        &joiner.canonical(),
        &oracle.canonical(),
        "a joiner with a live background task",
    );
    assert_no_dangling(&joiner.chat);
    assert_no_dangling(&oracle.chat);
    assert_eq!(
        joiner.canonical().tasks,
        oracle.canonical().tasks,
        "including where each task's output will paint",
    );
    fixture.shutdown().await;
}

/// A mid-session joiner learns the active settings from the attach `state`
/// frame, which is their only carrier: no projected event names them
/// (spec 6.3, 9.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mid_session_joiner_sees_the_active_settings() {
    let fixture = Fixture::new(vec![finalized_text_message("answered")]).await;
    let session = fixture.create().await;
    let mut oracle = fixture.oracle(&session).await;
    fixture.prompt(&session, "hi").await;
    oracle.settle().await;

    fixture
        .settings(
            &session,
            SessionSettings {
                thinking: Some("high".to_string()),
                ..SessionSettings::default()
            },
        )
        .await
        .expect("the change applies");
    oracle.settle().await;

    let mut joiner = fixture.remote(&session).await;
    joiner.settle().await;

    let footer = joiner
        .canonical()
        .agent(AgentId::Main)
        .expect("a main transcript")
        .settings
        .clone()
        .expect("the joiner's footer settings");
    assert_eq!(
        footer.thinking, "high",
        "the joiner renders the session's live settings, not its own defaults",
    );
    assert_converged(&joiner, &oracle, "settings visibility for a joiner");
    fixture.shutdown().await;
}

/// Deterministic distinct cut positions, drawn from a fixed seed so a failure
/// reproduces.
fn cuts(seed: u64, frames: usize, count: usize) -> Vec<usize> {
    let frames = frames.max(1);
    let mut state = seed;
    let mut cuts = Vec::with_capacity(count);
    while cuts.len() < count.min(frames) {
        // A plain LCG: reproducibility is the whole requirement here.
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let pick = usize::try_from(state >> 33).unwrap_or(0) % frames;
        if !cuts.contains(&pick) {
            cuts.push(pick);
        }
    }
    cuts
}

/// The provider a cut test runs, streaming in small chunks so the cut can
/// land inside a message as well as between two.
fn cut_provider(script: Vec<AssistantMessage>) -> Arc<ScriptedProvider> {
    scripted(script, 1, Duration::from_millis(2))
}

/// Cut the stream after `cut` live frames, re-attach with the cursor the fold
/// committed, and require convergence with the oracle.
///
/// Answers whether the cut left the client short of the oracle's durable
/// position, which is what makes a cut worth anything: a cut past the turn's
/// last durable entry converges trivially.
async fn converges_after_a_cut(script: Vec<AssistantMessage>, cut: usize) -> bool {
    let fixture = Fixture::with_provider(cut_provider(script)).await;
    let session = fixture.create().await;
    let mut oracle = fixture.oracle(&session).await;
    let mut remote = fixture.remote(&session).await;
    fixture.prompt(&session, "do the thing").await;

    let folded = remote.pump_frames(cut).await;
    remote.cut();
    let interrupted = remote.client.cursor().map(|cursor| cursor.seq);
    oracle.settle().await;
    let complete = oracle.client.cursor().map(|cursor| cursor.seq);
    remote.reattach().await;
    remote.settle().await;

    assert_eq!(
        main_rows(&oracle.canonical()),
        TURN_ROWS,
        "the compared state is a whole turn",
    );
    assert_converged(&remote, &oracle, &format!("a cut after {folded} frames"));
    fixture.shutdown().await;
    interrupted < complete
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_cut_at_seeded_boundaries_converges() {
    // How many live frames one turn produces, so the cuts land inside it.
    // Counted against the same provider the cut runs use, since a chunked
    // stream is what makes the frame count what it is.
    let frames = {
        let fixture = Fixture::with_provider(cut_provider(tool_turn())).await;
        let session = fixture.create().await;
        let mut remote = fixture.remote(&session).await;
        fixture.prompt(&session, "do the thing").await;
        let mut count = 0;
        remote
            .pump_until("the turn to finish", |frame| {
                count += 1;
                matches!(frame, Frame::State { working: false, .. })
            })
            .await;
        fixture.shutdown().await;
        count
    };
    assert!(frames > 5, "a tool turn is more than a handful of frames");

    let mut interrupted = 0;
    for cut in cuts(0x5eed_1234_9abc_def0, frames, 6) {
        interrupted += usize::from(converges_after_a_cut(tool_turn(), cut).await);
    }
    assert!(
        interrupted > 0,
        "every cut landed past the turn's last durable entry, which proves nothing",
    );

    // And the same for a turn with a sub-agent in it, whose bracketing is the
    // part a suffix projection has to get right.
    let mut interrupted = 0;
    for cut in cuts(0x1234_5678_9abc_def0, frames, 4) {
        interrupted += usize::from(converges_after_a_cut(sub_agent_turn(), cut).await);
    }
    assert!(
        interrupted > 0,
        "every sub-agent cut landed past the turn's last durable entry",
    );
}

/// The named sharp edge: the connection dies between a tool's
/// `ToolExecutionEnd` and the durable `MessageEnd` of its result entry, so
/// the client holds a finished cell whose log entry it never saw.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cut_between_a_tool_end_and_its_durable_message_converges() {
    let fixture = Fixture::new(tool_turn()).await;
    let session = fixture.create().await;
    let mut oracle = fixture.oracle(&session).await;
    let mut remote = fixture.remote(&session).await;
    fixture.prompt(&session, "check the todos").await;

    remote
        .pump_until("the tool call to end", |frame| {
            matches!(frame, Frame::Event { event, .. }
                if matches!(event.known(), Some(AgentEvent::ToolExecutionEnd { .. })))
        })
        .await;
    remote.cut();

    oracle.settle().await;
    remote.reattach().await;
    remote.settle().await;

    assert_eq!(main_rows(&oracle.canonical()), TURN_ROWS);
    assert_converged(&remote, &oracle, "a cut on the tool-end boundary");
    fixture.shutdown().await;
}

/// The named sharp edge: the connection dies with a tool call and a sub-agent
/// both in flight. The re-attach must not duplicate the tool cell (its
/// arguments live nowhere else, so quiesce keeps the cell and the backfill
/// cannot regenerate it) and must not leave either spinner stuck.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cut_with_a_tool_and_a_sub_agent_running_converges() {
    // A report long enough that the sub is still streaming it when the client
    // comes back, and a command that keeps the parent blocked past that.
    let fixture = Fixture::with_provider(cut_provider(running_tool_and_sub_turn(
        "the sub reporting back at length, one character at a time, so that its \
         run is still going when the client comes back",
        3,
    )))
    .await;
    let session = fixture.create().await;
    let mut oracle = fixture.oracle(&session).await;
    let mut remote = fixture.remote(&session).await;
    fixture.prompt(&session, "do both").await;

    // Cut once the slow command is under way. The sub-agent was spawned by the
    // same message and is streaming its answer, so both are in flight.
    remote
        .pump_until("the slow tool call to start", |frame| {
            matches!(frame, Frame::Event { event, .. }
                if matches!(event.known(),
                    Some(AgentEvent::ToolExecutionStart { call_id, .. })
                        if call_id == "call-slow"))
        })
        .await;
    remote.cut();
    remote.reattach().await;

    let state = remote.canonical();
    let cells = running_cells(&state, "call-slow");
    assert_eq!(
        cells.len(),
        1,
        "the re-attach neither dropped the running cell nor added a second: {state:?}",
    );
    assert_eq!(
        cells[0]["command"],
        serde_json::json!("sleep 3"),
        "quiesce kept the cell's arguments, which no backfill can regenerate",
    );
    assert_eq!(
        sub_box(&state, 1),
        (SubAgentStatus::Running, false),
        "the sub-agent's bracket is still open and its clock still running",
    );

    oracle.settle().await;
    remote.settle().await;

    let state = remote.canonical();
    assert!(
        state.running.is_empty(),
        "no spinner outlived the work: {:?}",
        state.running,
    );
    assert_eq!(
        sub_box(&state, 1).0,
        SubAgentStatus::Done,
        "the sub-agent's box concluded",
    );
    assert!(
        running_cells(&state, "call-slow").is_empty(),
        "and the slow call finished: {state:?}",
    );
    assert_converged(&remote, &oracle, "a cut with a tool and a sub in flight");
    fixture.shutdown().await;
}

/// The named sharp edge: a re-attach where zero durable entries follow the
/// cursor, yet a sub-agent concluded in the gap. Its `SubAgentEnd` is
/// reliable-transient, so what has to conclude the client's box is the block
/// itself (spec 6.5).
///
/// The gap is real here: the client's box is `Running` when it comes back. The
/// parent stays blocked on a slow command while the sub finishes, which is what
/// keeps a durable entry from landing behind the cursor and turning this into an
/// ordinary incremental resume. Two mechanisms then carry the conclusion, the
/// bracketing the projection closes for a run the host knows is finished, and
/// the post-`caught_up` sweep, and the test pins both: the first concludes the
/// box, the second lands on it without disturbing it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reattach_with_no_durable_suffix_still_concludes_a_sub_agent() {
    // A short report, so the sub concludes early in the parent's long sleep.
    let fixture =
        Fixture::with_provider(cut_provider(running_tool_and_sub_turn("done looking", 5))).await;
    let session = fixture.create().await;
    let mut oracle = fixture.oracle(&session).await;
    let mut remote = fixture.remote(&session).await;
    fixture.prompt(&session, "do both").await;

    // Up to the usage update trailing the sub-agent's assistant message, which
    // is its last durable entry and the last event that entry projects. So the
    // client has applied everything the log holds, and holds the entry back
    // from its committed cursor only because a trailing event might follow it
    // (spec 6.5).
    remote
        .pump_until("the sub-agent's usage update", |frame| {
            matches!(frame, Frame::Event { event, .. }
                if matches!(event.known(),
                    Some(AgentEvent::UsageUpdate { agent_id: AgentId::Sub(1), .. })))
        })
        .await;
    remote.cut();
    assert_eq!(
        sub_box(&remote.canonical(), 1).0,
        SubAgentStatus::Running,
        "the box is open when the connection dies",
    );

    // The sub concludes while this client is away. `SubAgentEnd` is
    // reliable-transient, so nothing replays it.
    oracle
        .pump_until("the sub-agent to conclude", |frame| {
            matches!(frame, Frame::Event { event, .. }
                if matches!(event.known(), Some(AgentEvent::SubAgentEnd { .. })))
        })
        .await;

    // The session's high-water mark, read off the host's own bookkeeping,
    // under the epoch the fold adopted. The client applied every entry up to
    // it, it only held the last one back from its committed cursor, so this is
    // the boundary the reattach below has to be served at. A client may not
    // turn a position it read in the directory into a cursor (spec 6.5), the
    // test is reading ground truth to build the case.
    let epoch = remote.client.cursor().expect("a committed cursor").epoch;
    let last_seq = fixture
        .client
        .sessions()
        .await
        .expect("the sessions read")
        .sessions
        .iter()
        .find(|entry| entry.id == session)
        .expect("the session")
        .last_seq
        .expect("a live session's row reports its position");

    let block = remote
        .reattach_at(Some(Cursor {
            epoch,
            seq: last_seq,
        }))
        .await;

    assert!(
        !block.iter().any(|frame| frame.durable_seq().is_some()),
        "the suffix at the high-water mark is empty: {block:?}",
    );
    assert_eq!(
        sub_box(&remote.canonical(), 1).0,
        SubAgentStatus::Done,
        "the block's bracketing closed the box the client came back with: {block:?}",
    );
    let sweep = remote
        .pump_until("the conclusion sweep", |frame| {
            matches!(frame, Frame::Event { event, .. }
                if matches!(event.known(),
                    Some(AgentEvent::AgentEnd { agent_id: AgentId::Sub(1), .. })))
        })
        .await;
    assert!(
        sweep.iter().all(|frame| frame.durable_seq().is_none()),
        "the sweep is synthesized, so it carries no cursor: {sweep:?}",
    );
    assert_eq!(
        sub_box(&remote.canonical(), 1).0,
        SubAgentStatus::Done,
        "and the sweep behind it is idempotent",
    );

    oracle.settle().await;
    remote.settle().await;
    assert_converged(&remote, &oracle, "a sweep over a box open across the gap");
    fixture.shutdown().await;
}

/// A head switch mints a fresh epoch, so a client that has not re-attached
/// drops everything the new branch produces (spec 6.5). Once it does
/// re-attach, the full backfill lands it where a client that only ever saw
/// the new branch is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frames_from_a_stale_epoch_are_dropped_until_a_reattach() {
    let fixture = Fixture::new(vec![
        finalized_text_message("first answer"),
        finalized_text_message("second answer"),
        finalized_text_message("third answer"),
    ])
    .await;
    let session = fixture.create().await;
    let mut remote = fixture.remote(&session).await;
    fixture.prompt(&session, "one").await;
    remote.settle().await;
    fixture.prompt(&session, "two").await;
    remote.settle().await;

    // Branch at the head after the first turn. The entry id comes from the
    // log, which is where a tree view gets it too.
    let target = {
        let handles = fixture
            .host
            .local_handles(&session)
            .await
            .expect("live session");
        let log = handles.log.lock().await;
        let head = log.head().cloned().expect("a head");
        log.linearize(&head, ThreadFilter::USER)
            .entries()
            .iter()
            .rev()
            .nth(2)
            .expect("an earlier entry")
            .id
            .clone()
    };

    fixture
        .client
        .command(
            &session,
            &RemoteCommand::Head(HeadRequest::entry(target.clone())),
        )
        .await
        .expect("a head switch on an idle session");
    remote
        .pump_until("the reset frame", |frame| {
            matches!(frame, Frame::Reset { .. })
        })
        .await;
    assert!(
        remote.client.needs_reattach(),
        "a reset leaves the client owing a re-attach",
    );
    let before = remote.canonical();

    // A whole turn under the new epoch, which this client must not apply.
    let mut fresh = fixture.remote(&session).await;
    fixture.prompt(&session, "three").await;
    fresh.settle().await;
    remote.settle().await;
    assert_canonical_eq(
        &remote.canonical(),
        &before,
        "every frame of the new epoch was dropped",
    );

    // The stale cursor is still safe to offer: the server decides what it can
    // serve from it, and here that is everything.
    remote.reattach().await;
    remote.settle().await;
    assert_converged(&remote, &fresh, "a full backfill under the new epoch");
    assert_eq!(
        remote.client.cursor().map(|cursor| cursor.epoch),
        fresh.client.cursor().map(|cursor| cursor.epoch),
        "the client adopted the epoch of the block it was served",
    );
    fixture.shutdown().await;
}
