//! Two in-process hosts behind a gateway (spec 11.5).
//!
//! Everything here runs real hosts on real loopback ports behind a real
//! gateway router on a third one, so the bytes under test are the bytes a
//! client of a gateway sees. The hosts come from the same recipe the transport
//! tests use ([`crate::remote::tests::scripted_host`]), which is what keeps
//! "a test host" one thing in this crate.
//!
//! A gateway is eventually consistent with its hosts by construction: it
//! learns their directories from streams. Assertions about the merged
//! directory therefore poll it ([`Fixture::until`]) rather than assuming a
//! moment, and every wait goes through [`bounded`] so a wedged link fails a test
//! instead of hanging CI.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_app::cli::args::Args;
use aj_app::host::{AttachRequest, Command, SessionHost};
use aj_app::test_support::finalized_text_message;
use aj_models::types::AssistantContent;
use aj_wire::{
    CreateSessionRequest, Cursor, DecodedFrame, DirectoryHost, EnrollHostRequest, ErrorResponse,
    Frame, HostList, HostSource, HostSummary, PROTOCOL_VERSION, PromptInput, PromptRequest,
    SessionCreated, SessionList, SessionSummary,
};
use reqwest::StatusCode;
use tempfile::TempDir;

use super::*;
use crate::gateway::naming::SessionAddress;
use crate::remote::tests::{
    FakeWhois, HostHandles, addr, bounded, canned_server, scripted, scripted_host,
};
use crate::remote::{IdentityGate, RemoteClient, RemoteCommand, RemoteEvents, RemoteServer};

/// How long a settled directory has to prove it is not settled after all.
///
/// Longer than a host's own `list` coalescing tick (200ms), short enough that
/// a test that only wants quiet stays cheap.
const QUIET: Duration = Duration::from_millis(400);

/// Tuning that makes a link's reconnect visible inside a test's patience.
///
/// The production delays are seconds, which is right for a tailnet and far too
/// slow to watch. The stream-silence timeout is left at the client's default:
/// the one host here that goes quiet on an open stream is only up for a moment,
/// and every test finishes long before sixty seconds of silence.
fn tuning() -> Tuning {
    Tuning {
        reconnect_delay: Duration::from_millis(20),
        max_reconnect_delay: Duration::from_millis(100),
        ..Tuning::default()
    }
}

/// One host of the fixture: its store, its scripted provider, and the loopback
/// server in front of it.
///
/// The store outlives a stop, which is what makes [`Self::restart`] the same
/// store under a new process rather than a different host.
struct Upstream {
    dir: TempDir,
    host: SessionHost,
    server: Option<RemoteServer>,
    addr: SocketAddr,
}

impl Upstream {
    async fn start() -> Self {
        Self::named(None).await
    }

    /// A host calling itself `name`, or deriving one from its working directory
    /// where that is `None`.
    async fn named(name: Option<&str>) -> Self {
        let dir = TempDir::new().expect("tempdir");
        let (host, server) = Self::serve(&dir, None, name).await;
        let addr = server.local_addr();
        Self {
            dir,
            host,
            server: Some(server),
            addr,
        }
    }

    /// A host over `dir`'s store, served on `at` or on a fresh loopback port,
    /// calling itself `name` or deriving a name from `dir` (spec 6.1).
    async fn serve(
        dir: &TempDir,
        at: Option<SocketAddr>,
        name: Option<&str>,
    ) -> (SessionHost, RemoteServer) {
        let provider = scripted(
            vec![
                finalized_text_message("done"),
                finalized_text_message("done"),
            ],
            0,
            Duration::ZERO,
        );
        let host = scripted_host(dir, provider, HostHandles::new(dir), name);
        let server = RemoteServer::bind(
            host.clone(),
            at.unwrap_or_else(|| addr("127.0.0.1:0")),
            IdentityGate::local(),
        )
        .await
        .expect("bind a loopback control port");
        (host, server)
    }

    fn address(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn host_id(&self) -> String {
        self.host.hello().host_id
    }

    /// The name this host reports for itself, which a real host always has: it
    /// derives one from its working directory when nothing named it (spec 6.1).
    fn host_name(&self) -> String {
        self.host
            .hello()
            .name
            .expect("a host names itself, from its working directory if nothing else")
    }

    /// A namespaced id for one of this host's sessions, as a gateway client
    /// addresses it.
    fn namespaced(&self, session: &str) -> String {
        format!("{}:{session}", self.host_id())
    }

    async fn create(&self) -> String {
        self.host.create().await.expect("create a session")
    }

    /// The sessions this host itself reports, which is where a create either
    /// landed or did not.
    ///
    /// Read from the host rather than through the gateway, for the same reason
    /// [`Self::durable_seq`] is.
    async fn session_ids(&self) -> Vec<String> {
        let list = self.host.sessions().await.expect("the host's directory");
        list.sessions.into_iter().map(|row| row.id).collect()
    }

    /// How far `session`'s log has got, as this host itself reports it.
    ///
    /// Read from the host rather than through the gateway, so an assertion about
    /// where a command landed does not depend on the thing under test.
    async fn durable_seq(&self, session: &str) -> u64 {
        let list = self.host.sessions().await.expect("the host's directory");
        list.sessions
            .iter()
            .find(|row| row.id == session)
            .expect("the session is this host's")
            .last_seq
            .expect("a live row carries its position")
    }

    /// Take the host down, in the ordinary teardown order: the host closes its
    /// streams, then the server stops accepting.
    async fn stop(&mut self) {
        self.host.shutdown().await;
        if let Some(server) = self.server.take() {
            server.shutdown().await;
        }
    }

    /// Bring the same store back on the same address.
    ///
    /// The address is reused deliberately: a gateway redials the address it was
    /// enrolled with, so a fresh port would test enrollment instead of
    /// reconnection. The store carries the `host-id` file, so the host returns
    /// under the same id and its sessions keep their namespace. Their epochs do
    /// not: an epoch is minted per materialization and never persisted (spec
    /// 6.5), so a restart is what makes a cursor stale.
    async fn restart(&mut self) {
        self.restart_as(None).await;
    }

    /// The same, with the host calling itself `name` this time round: what a
    /// gateway meets when an operator restarts a host under a new one.
    async fn restart_as(&mut self, name: Option<&str>) {
        let (host, server) = Self::serve(&self.dir, Some(self.addr), name).await;
        self.host = host;
        self.server = Some(server);
    }

    /// Run a turn with no gateway in the way.
    ///
    /// What a test needs while the gateway's connection to this host is cut: the
    /// host is up and its sessions keep running, which is exactly the state an
    /// incremental resume is about.
    async fn prompt(&self, session: &str, text: &str) {
        self.host
            .command(
                session,
                Command::Prompt {
                    agent: AgentId::Main,
                    content: PromptInput::Text {
                        text: text.to_string(),
                    }
                    .into_content(),
                },
            )
            .await
            .expect("the host accepts the prompt");
    }
}

/// A loopback relay in front of a host, whose connections a test can cut.
///
/// The gateway is enrolled at the relay's address, so a cut breaks every
/// connection this gateway holds to that host, control link and spliced streams
/// alike, while the host itself keeps running. That is the flap spec 7.1
/// describes: "a gateway-to-host connection drops ... even though client
/// connections stayed up", and the one where the host's epochs survive, so a
/// resume through it is incremental.
struct Bridge {
    address: HostAddress,
    /// Whether connections are passed through, and the pipes in flight.
    pipes: Arc<StdMutex<Pipes>>,
    accepting: tokio::task::JoinHandle<()>,
}

/// What a bridge is doing for the connections through it.
///
/// One lock over both fields, so a cut cannot land between the decision to pipe
/// a connection and the handle that would abort it. A pipe registered after the
/// cut drained the list would carry its connection on, and the flap the test
/// asked for would never happen: the gateway would keep its stream, the `reset`
/// would never come, and the wait for it would end at the deadline.
struct Pipes {
    open: bool,
    piping: Vec<tokio::task::AbortHandle>,
}

impl Bridge {
    async fn to(upstream: &Upstream) -> Self {
        let listener = tokio::net::TcpListener::bind(addr("127.0.0.1:0"))
            .await
            .expect("bind a bridge port");
        let bound = listener.local_addr().expect("local addr");
        let target = upstream.addr;
        let pipes = Arc::new(StdMutex::new(Pipes {
            open: true,
            piping: Vec::new(),
        }));
        let accepting = tokio::spawn({
            let pipes = Arc::clone(&pipes);
            async move {
                loop {
                    let inbound = match listener.accept().await {
                        Ok((inbound, _)) => inbound,
                        // A dial this bridge cut mid-handshake arrives as an
                        // error and says nothing about the listener. Ending the
                        // loop over one would leave every later dial hanging on a
                        // bridge that stopped accepting, which is a test timing
                        // out on a wait its own fixture broke. Paced so that a
                        // listener that really is done cannot spin.
                        Err(_) => {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                            continue;
                        }
                    };
                    let mut held = pipes.lock().expect("the bridge mutex is poisoned");
                    // Accepted and closed at once while cut, so a dial fails
                    // rather than hanging on a connection nothing answers.
                    if !held.open {
                        continue;
                    }
                    let pipe = tokio::spawn(async move {
                        let mut inbound = inbound;
                        let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await else {
                            return;
                        };
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    });
                    held.piping.push(pipe.abort_handle());
                }
            }
        });
        Self {
            address: HostAddress::parse(&format!("http://{bound}")).expect("an address"),
            pipes,
            accepting,
        }
    }

    /// Break every connection through the bridge, and refuse new ones.
    fn cut(&self) {
        let mut held = self.pipes.lock().expect("the bridge mutex is poisoned");
        held.open = false;
        for pipe in held.piping.drain(..) {
            pipe.abort();
        }
    }

    /// Let connections through again.
    fn heal(&self) {
        self.pipes
            .lock()
            .expect("the bridge mutex is poisoned")
            .open = true;
    }

    fn stop(self) {
        self.cut();
        self.accepting.abort();
    }
}

/// A gateway over a temp state directory, bound on loopback, plus a client for
/// it.
struct Fixture {
    state: TempDir,
    /// The addresses this gateway's configuration names, which a restart keeps:
    /// the configuration file does not change because the process did (spec 7.1).
    static_hosts: Vec<HostAddress>,
    tuning: Tuning,
    gateway: Gateway,
    server: GatewayServer,
    client: RemoteClient,
    http: reqwest::Client,
}

impl Fixture {
    /// A gateway with `static_hosts` in its configuration and nothing enrolled
    /// dynamically.
    async fn new(static_hosts: &[&Upstream]) -> Self {
        let state = TempDir::new().expect("tempdir");
        let addresses = static_hosts
            .iter()
            .map(|host| HostAddress::parse(&host.address()).expect("a host address"))
            .collect();
        Self::over(state, addresses).await
    }

    async fn over(state: TempDir, static_hosts: Vec<HostAddress>) -> Self {
        Self::tuned(state, static_hosts, tuning()).await
    }

    async fn tuned(state: TempDir, static_hosts: Vec<HostAddress>, tuning: Tuning) -> Self {
        let gateway = Gateway::new(GatewaySetup {
            state_dir: state.path().to_path_buf(),
            static_hosts: static_hosts.clone(),
            tuning,
        })
        .expect("a gateway over a fresh state directory");
        let server =
            GatewayServer::bind(gateway.clone(), addr("127.0.0.1:0"), IdentityGate::local())
                .await
                .expect("bind a loopback gateway port");
        let client = RemoteClient::new(&server.url()).expect("client");
        Self {
            state,
            static_hosts,
            tuning,
            gateway,
            server,
            client,
            http: reqwest::Client::new(),
        }
    }

    /// The same gateway again over the same state directory and the same
    /// configuration: a restart, with nothing but those carried across.
    async fn restart(self) -> Self {
        let static_hosts = self.static_hosts.clone();
        self.restarted_over(static_hosts).await
    }

    /// The same, with the configuration the operator now has: what editing that
    /// file and restarting does.
    async fn restarted_over(self, static_hosts: Vec<HostAddress>) -> Self {
        let Self {
            state,
            tuning,
            gateway,
            server,
            ..
        } = self;
        server.shutdown().await;
        gateway.shutdown().await;
        Self::tuned(state, static_hosts, tuning).await
    }

    /// Poll the merged directory until `check` answers, which is how every
    /// assertion about it waits for a link to deliver.
    async fn until<T>(&self, what: &str, mut check: impl FnMut(&SessionList) -> Option<T>) -> T {
        bounded(what, async {
            loop {
                let list = self.client.sessions().await.expect("the merged directory");
                if let Some(found) = check(&list) {
                    return found;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
    }

    /// The row for one namespaced id, once it is there.
    async fn row(&self, id: &str) -> SessionSummary {
        self.until(&format!("the row for {id}"), |list| {
            list.sessions.iter().find(|row| row.id == id).cloned()
        })
        .await
    }

    async fn hosts(&self) -> HostList {
        let response = self
            .http
            .get(format!("{}/v1/hosts", self.server.url()))
            .send()
            .await
            .expect("the hosts read");
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.expect("a host list")
    }

    /// Poll the enrolled hosts until `check` answers.
    async fn until_hosts<T>(&self, what: &str, mut check: impl FnMut(&HostList) -> Option<T>) -> T {
        bounded(what, async {
            loop {
                if let Some(found) = check(&self.hosts().await) {
                    return found;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
    }

    /// Wait until the control connection to `id` is up.
    ///
    /// What a create waits for: a target this gateway cannot reach is refused
    /// rather than held, so a create raced against the first dial would be a
    /// 503 about nothing.
    async fn until_connected(&self, id: &str) {
        self.until_hosts(&format!("the host {id} to answer"), |hosts| {
            hosts
                .hosts
                .iter()
                .find(|host| host.id.as_deref() == Some(id) && host.connected)
                .map(|_| ())
        })
        .await;
    }

    /// Create a session, sending `body` verbatim, so a test can send a field
    /// this build does not know and a number no float holds.
    async fn create(&self, body: &str) -> reqwest::Response {
        self.create_with_query("", body).await
    }

    /// The same with `query` (no `?`) on the request: a create's parameters are
    /// as much the client's as its body (spec 6.10).
    async fn create_with_query(&self, query: &str, body: &str) -> reqwest::Response {
        let separator = if query.is_empty() { "" } else { "?" };
        self.http
            .post(format!(
                "{}/v1/sessions{separator}{query}",
                self.server.url()
            ))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await
            .expect("the create request")
    }

    /// Enroll `address`, answering whatever the gateway replied.
    async fn enroll(&self, address: &str) -> reqwest::Response {
        self.http
            .post(format!("{}/v1/hosts", self.server.url()))
            .json(&EnrollHostRequest {
                address: address.to_string(),
            })
            .send()
            .await
            .expect("the enrollment request")
    }

    async fn withdraw(&self, id: &str) -> reqwest::Response {
        self.http
            .delete(format!("{}/v1/hosts/{id}", self.server.url()))
            .send()
            .await
            .expect("the withdrawal request")
    }

    /// Open a client stream attaching `sessions`, by their namespaced ids.
    async fn attach(&self, sessions: &[AttachRequest]) -> RemoteEvents {
        self.client
            .events(sessions)
            .await
            .expect("a client stream onto the gateway")
    }

    async fn shutdown(self) {
        self.server.shutdown().await;
        self.gateway.shutdown().await;
        drop(self.state);
    }
}

/// One session to attach, with no cursor.
fn attach(session: &str) -> AttachRequest {
    AttachRequest {
        session: session.to_string(),
        cursor: None,
    }
}

/// One session to attach, offering `cursor`.
fn attach_at(session: &str, cursor: Cursor) -> AttachRequest {
    AttachRequest {
        session: session.to_string(),
        cursor: Some(cursor),
    }
}

/// Read frames until `done` accepts one, answering everything read, that frame
/// included.
async fn frames_until(
    events: &mut RemoteEvents,
    what: &str,
    mut done: impl FnMut(&Frame) -> bool,
) -> Vec<Frame> {
    let mut seen = Vec::new();
    bounded(what, async {
        loop {
            let Some(frame) = events.recv().await else {
                panic!("the stream ended before {what}, having carried {seen:?}");
            };
            let frame = frame.unwrap_or_else(|err| panic!("a good frame: {err}"));
            let stop = done(&frame);
            seen.push(frame);
            if stop {
                return;
            }
        }
    })
    .await;
    seen
}

/// Every frame that arrives inside `window`, which is how a test asserts that
/// something does *not*.
async fn frames_within(events: &mut RemoteEvents, window: Duration) -> Vec<Frame> {
    let mut seen = Vec::new();
    let collecting = async {
        while let Some(frame) = events.recv().await {
            seen.push(frame.expect("a good frame"));
        }
    };
    let _ = tokio::time::timeout(window, collecting).await;
    seen
}

/// What a stream carried, as counts and names rather than frames.
///
/// For the tests that read an attach block deep enough to fill the sockets
/// between a host and a client ([`deep_block`]): tens of megabytes, which a test
/// that kept the frames would print back at a failed assertion. This keeps what
/// those tests assert on, the eviction they are about included, because an
/// evicted stream ends.
#[derive(Debug, Default)]
struct Carried {
    /// How many `event` frames arrived, by session.
    events: BTreeMap<String, usize>,
    /// The sessions whose `caught_up` arrived, in order.
    caught_up: Vec<String>,
    /// The sessions a `reset` named, in order.
    resets: Vec<String>,
    /// Whether the stream ended, which for a client of a gateway means it was
    /// evicted or the gateway went away (spec 6.9).
    ended: bool,
}

impl Carried {
    fn events(&self, session: &str) -> usize {
        self.events.get(session).copied().unwrap_or_default()
    }
}

/// Read frames into a tally until `done` accepts it, or the stream ends.
async fn carried_until(
    events: &mut RemoteEvents,
    what: &str,
    mut done: impl FnMut(&Carried) -> bool,
) -> Carried {
    let mut carried = Carried::default();
    bounded(what, async {
        while !done(&carried) {
            let Some(frame) = events.recv().await else {
                carried.ended = true;
                return;
            };
            match frame.unwrap_or_else(|err| panic!("a good frame: {err}")) {
                Frame::Event { session, .. } => *carried.events.entry(session).or_default() += 1,
                Frame::CaughtUp { session, .. } => carried.caught_up.push(session),
                Frame::Reset { session } => carried.resets.push(session),
                _ => {}
            }
        }
    })
    .await;
    carried
}

/// The sessions the `reset` frames among `frames` name (spec 6.3).
fn resets(frames: &[Frame]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Reset { session } => Some(session.clone()),
            _ => None,
        })
        .collect()
}

/// The sessions the `error` frames among `frames` refuse (spec 6.3).
fn refused_sessions<'a>(frames: impl Iterator<Item = &'a Frame>) -> Vec<&'a str> {
    frames
        .filter_map(|frame| match frame {
            Frame::Error { session, .. } => Some(session.as_str()),
            _ => None,
        })
        .collect()
}

/// The first `error` frame on `events`: the session it refuses, its code and
/// its message (spec 6.3).
async fn refused_session(events: &mut RemoteEvents) -> (String, String, String) {
    let frames = frames_until(events, "a session-scoped refusal", |frame| {
        matches!(frame, Frame::Error { .. })
    })
    .await;
    let Some(Frame::Error {
        session,
        code,
        message,
        ..
    }) = frames.into_iter().next_back()
    else {
        unreachable!("the read stops on an error frame")
    };
    (session, code, message)
}

/// The concatenated text of every finalized assistant message among `frames`.
fn assistant_text(frames: &[Frame]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Event { event, .. } => event.known(),
            _ => None,
        })
        .filter_map(|event| match event {
            AgentEvent::MessageEnd { message, .. } => message.as_stored_wire(),
            _ => None,
        })
        .filter_map(|message| match message {
            aj_models::types::Message::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .map(|assistant| {
            assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    AssistantContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .collect()
}

/// The durable positions among `frames`, in delivery order (spec 6.4).
fn durable_seqs(frames: &[Frame]) -> Vec<u64> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Event {
                durability: Some(durability),
                ..
            } => Some(durability.seq),
            _ => None,
        })
        .collect()
}

/// The epoch of the `state` frame that opens an attach block (spec 6.5).
fn epoch_of(frames: &[Frame]) -> String {
    frames
        .iter()
        .find_map(|frame| match frame {
            Frame::State { epoch, .. } => Some(epoch.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("an attach block opens with a state frame: {frames:?}"))
}

/// The `caught_up` position of an attach block, and the cursor a client would
/// commit from it.
fn caught_up_at(frames: &[Frame]) -> u64 {
    frames
        .iter()
        .find_map(|frame| match frame {
            Frame::CaughtUp { last_seq, .. } => Some(*last_seq),
            _ => None,
        })
        .unwrap_or_else(|| panic!("an attach block ends with caught_up: {frames:?}"))
}

/// Every session id the session-scoped frames among `frames` name.
fn named_sessions(frames: &[Frame]) -> Vec<&str> {
    frames.iter().filter_map(Frame::session).collect()
}

/// The `{code, message}` body behind a refusal.
async fn refusal(response: reqwest::Response) -> (StatusCode, String, String) {
    let status = response.status();
    let body: ErrorResponse = response.json().await.expect("the protocol's error shape");
    (status, body.code, body.message)
}

fn prompt(text: &str) -> RemoteCommand {
    RemoteCommand::Prompt(PromptRequest {
        agent: None,
        input: PromptInput::Text {
            text: text.to_string(),
        },
    })
}

// ---------------------------------------------------------------------------
// Namespacing and the merged directory (spec 6.2, 6.8, 7.1)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_only_ever_sees_namespaced_ids() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let session = left.create().await;
    let other = right.create().await;
    let fixture = Fixture::new(&[&left, &right]).await;

    let rows = fixture
        .until("both hosts' sessions", |list| {
            (list.sessions.len() == 2).then(|| list.sessions.clone())
        })
        .await;

    for row in &rows {
        let address = SessionAddress::parse(&row.id).unwrap_or_else(|err| {
            panic!(
                "a gateway row carries a namespaced id, got {:?}: {err}",
                row.id
            )
        });
        assert_eq!(
            row.host.as_deref(),
            Some(address.host.as_str()),
            "the row names its host, so a client never parses the id (spec 6.8)",
        );
        assert!(!row.unreachable, "both hosts are up");
    }
    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert!(ids.contains(&left.namespaced(&session).as_str()), "{ids:?}");
    assert!(ids.contains(&right.namespaced(&other).as_str()), "{ids:?}");
    assert!(
        !ids.contains(&session.as_str()) && !ids.contains(&other.as_str()),
        "a bare host-local id never reaches a client: {ids:?}",
    );

    fixture.shutdown().await;
    left.stop().await;
    right.stop().await;
}

/// A row travels as the host that owns it wrote it (spec 6.10): the gateway
/// rewrites the three fields it owns and passes everything else through, a field
/// this build has no type for and a number literal no float survives included.
///
/// Two hosts, because the merge is where two hosts' rows meet and is the one
/// place a re-encode would be tempting. The `preview` this carries is the field
/// spec section 13 banks as future work, which is exactly the version ceiling
/// spec 6.10 exists to prevent: a gateway must forward it years before it has a
/// type for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_newer_hosts_row_reaches_a_client_whole() {
    let left = FakeHost::with_rows(
        "left",
        Script::Frames(Vec::new()),
        vec![newer_row(
            &fake_row("s-2"),
            r#""preview":{"text":"auth","weight":18446744073709551616}"#,
        )],
    )
    .await;
    // A row that already names a host and calls itself unreachable: those are
    // this gateway's fields to answer for, whoever wrote them last.
    let right = FakeHost::with_rows(
        "right",
        Script::Frames(Vec::new()),
        vec![newer_row(
            &SessionSummary {
                host: Some("stale".to_string()),
                unreachable: true,
                ..fake_row("s-1")
            },
            r#""preview":"1e400 is a number too""#,
        )],
    )
    .await;
    let fixture = Fixture::tuned(
        TempDir::new().expect("tempdir"),
        vec![left.address.clone(), right.address.clone()],
        tuning(),
    )
    .await;
    fixture
        .until("both hosts' rows", |list| {
            (list.sessions.len() == 2).then_some(())
        })
        .await;

    let mut events = fixture.attach(&[]).await;
    let seen = decoded_until(&mut events, "the merged directory", |frame| {
        list_frame(frame).is_some_and(|(rows, _)| rows.len() == 2)
    })
    .await;
    let (carried, _) = seen
        .iter()
        .rev()
        .find_map(list_frame)
        .expect("a list frame with both rows");

    // The rows first: what a lost row costs is the whole session, and a preview
    // assertion on a directory of one would name the wrong harm.
    assert_eq!(
        carried
            .iter()
            .map(|row| (row.id.as_str(), row.host.as_deref(), row.unreachable))
            .collect::<Vec<_>>(),
        vec![
            ("left:s-2", Some("left"), false),
            ("right:s-1", Some("right"), false),
        ],
        "the three fields this gateway owns, and nobody else's answer for them",
    );
    let json = raw_json(&seen);
    assert!(
        json.contains(r#""preview":{"text":"auth","weight":18446744073709551616}"#),
        "a field this gateway has no type for reaches the client whole, literals \
         included: {json}",
    );
    assert!(
        json.contains(r#""preview":"1e400 is a number too""#),
        "and so does the other host's, which the merge saw in the same frame: {json}",
    );

    // The sessions read is the same composition, so it carries the same rows.
    let read = fixture
        .http
        .get(format!("{}/v1/sessions", fixture.server.url()))
        .send()
        .await
        .expect("the sessions read")
        .text()
        .await
        .expect("a body");
    assert!(
        read.contains(r#""preview":{"text":"auth","weight":18446744073709551616}"#)
            && read.contains(r#""id":"left:s-2""#),
        "a client that reads the directory sees what a client watching it sees: {read}",
    );

    fixture.shutdown().await;
    left.stop();
    right.stop();
}

/// A directory row as a host a version ahead writes it: everything this build
/// knows, plus `extra` spliced in as the text that host wrote.
fn newer_row(row: &SessionSummary, extra: &str) -> String {
    let known = serde_json::to_string(row).expect("a row");
    format!("{},{extra}}}", &known[..known.len() - 1])
}

/// An archived row reaches a client archived. The bit is not one of the three
/// fields a gateway owns, so it travels as the host wrote it, through the merge
/// where two hosts' rows meet.
///
/// Two hosts, and only one of them archived, so a merge that set the field for
/// everyone would fail here rather than pass by accident.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_archived_row_reaches_a_client_archived() {
    let left = FakeHost::with_rows(
        "left",
        Script::Frames(Vec::new()),
        vec![
            serde_json::to_string(&SessionSummary {
                archived: true,
                ..fake_row("s-2")
            })
            .expect("a row"),
        ],
    )
    .await;
    let right = FakeHost::with_rows(
        "right",
        Script::Frames(Vec::new()),
        vec![serde_json::to_string(&fake_row("s-1")).expect("a row")],
    )
    .await;
    let fixture = Fixture::tuned(
        TempDir::new().expect("tempdir"),
        vec![left.address.clone(), right.address.clone()],
        tuning(),
    )
    .await;
    fixture
        .until("both hosts' rows", |list| {
            (list.sessions.len() == 2).then_some(())
        })
        .await;

    let mut events = fixture.attach(&[]).await;
    let seen = decoded_until(&mut events, "the merged directory", |frame| {
        list_frame(frame).is_some_and(|(rows, _)| rows.len() == 2)
    })
    .await;
    let (carried, _) = seen
        .iter()
        .rev()
        .find_map(list_frame)
        .expect("a list frame with both rows");
    assert_eq!(
        carried
            .iter()
            .map(|row| (row.id.as_str(), row.archived))
            .collect::<Vec<_>>(),
        vec![("left:s-2", true), ("right:s-1", false)],
        "the archived host's row is archived and the other host's is not",
    );

    // The sessions read is the same composition, so it carries the same rows.
    let read: SessionList = fixture
        .http
        .get(format!("{}/v1/sessions", fixture.server.url()))
        .send()
        .await
        .expect("the sessions read")
        .json()
        .await
        .expect("a directory");
    assert_eq!(
        read.sessions
            .iter()
            .map(|row| (row.id.as_str(), row.archived))
            .collect::<Vec<_>>(),
        vec![("left:s-2", true), ("right:s-1", false)],
        "a client that reads the directory sees what a client watching it sees",
    );

    fixture.shutdown().await;
    left.stop();
    right.stop();
}

/// An archive lands on the host that owns the session, through a gateway that
/// knows nothing about the route: per-session requests are proxied by the
/// namespace, not by an enumeration of the routes this build has heard of
/// (spec 6.10, 7.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_archive_reaches_the_owning_host_through_a_gateway() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let session = left.create().await;
    let elsewhere = right.create().await;
    let fixture = Fixture::new(&[&left, &right]).await;
    fixture.row(&left.namespaced(&session)).await;

    let response = fixture
        .http
        .post(format!(
            "{}/v1/sessions/{}/archive",
            fixture.server.url(),
            left.namespaced(&session)
        ))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(r#"{"archived":true}"#)
        .send()
        .await
        .expect("the archive request");

    // The owning host's own directory is read before the response is unwrapped,
    // so a request that never arrived fails on the session it did not archive
    // rather than on the shape of the answer that came back.
    assert_eq!(
        archived_ids(&left).await,
        vec![session.clone()],
        "the archive landed on the session it named",
    );
    assert!(
        archived_ids(&right).await.is_empty(),
        "and on no session of the host that owns nothing here: {elsewhere}",
    );
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    // The gateway learns its rows from the host's stream, so the merged
    // directory catches up rather than being current the moment the POST
    // answers.
    fixture
        .until("the merged row to be archived", |list| {
            list.sessions
                .iter()
                .find(|row| row.id == left.namespaced(&session) && row.archived)
                .map(|_| ())
        })
        .await;

    fixture.shutdown().await;
    left.stop().await;
    right.stop().await;
}

/// The archived sessions one host reports about itself, with no gateway in the
/// way.
async fn archived_ids(host: &Upstream) -> Vec<String> {
    let list = host.host.sessions().await.expect("the host's directory");
    list.sessions
        .into_iter()
        .filter(|row| row.archived)
        .map(|row| row.id)
        .collect()
}

/// A gateway marks a host's sessions unreachable while it still has their rows,
/// and after a restart it has none: it stores no rows, deliberately (spec 7.1).
/// The signal survives because its `list` frames name the enrolled hosts with
/// their reachability, so a client that holds no rows for a host still knows the
/// host is there and cannot be reached.
///
/// Two hosts, one of them up, because the harm this is about is a host reading
/// as absent rather than as unreachable: with one host a client cannot tell an
/// empty directory from a directory that has not arrived, and the reachable
/// host's rows are what say the merge ran at all.
///
/// The downed one is the *configured* host, which is the enrollment mechanism
/// spec 7.1 lists first and the one an id has to outlive to be there at all: a
/// configured host is enrolled by address, so its id is only ever learned, and a
/// gateway that forgot it would come back with a host it cannot name, cannot
/// namespace and therefore cannot show.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_host_survives_a_restart_as_a_group_with_no_rows() {
    let mut down = Upstream::start().await;
    let mut up = Upstream::start().await;
    let session = down.create().await;
    let kept = up.create().await;
    // A turn on the session that is about to go away, so its host has a log to
    // enumerate when it comes back: the point here is a directory the *gateway*
    // no longer holds, not one the host lost.
    down.prompt(&session, "first").await;
    settled(&down, &session, 1).await;
    // One host each way, because a learned id has to survive whichever way the
    // host was enrolled (spec 7.1). The configured one is connected before the
    // second is enrolled, so its id is learned before anything else writes the
    // gateway's record: what this test is about is the restart, not the moment
    // the record happens to be written.
    let fixture = Fixture::new(&[&down]).await;
    fixture.until_connected(&down.host_id()).await;
    assert_eq!(fixture.enroll(&up.address()).await.status(), StatusCode::OK);
    let (lost, kept) = (down.namespaced(&session), up.namespaced(&kept));
    fixture.row(&lost).await;
    fixture.row(&kept).await;

    down.stop().await;
    let fixture = fixture.restart().await;

    let mut events = fixture.attach(&[]).await;
    let seen = decoded_until(&mut events, "the directory after the restart", |frame| {
        list_frame(frame)
            .is_some_and(|(rows, hosts)| rows.iter().any(|row| row.id == kept) && hosts.len() == 2)
    })
    .await;
    let (rows, hosts) = seen
        .iter()
        .rev()
        .find_map(list_frame)
        .expect("a list frame naming both hosts");

    // The rows first, because this measures nothing unless the gateway really
    // came back without them: with the downed host's rows still here, this is
    // the ordinary unreachable-row case and says nothing about a restart.
    assert!(
        !rows
            .iter()
            .any(|row| row.host.as_deref() == Some(&down.host_id())),
        "the gateway persisted the downed host's rows, so nothing here is about \
         a directory it no longer holds: {rows:?}",
    );
    let mut named = vec![
        DirectoryHost {
            id: Some(down.host_id()),
            address: None,
            name: Some(down.host_name()),
            unreachable: true,
        },
        DirectoryHost {
            id: Some(up.host_id()),
            address: None,
            name: Some(up.host_name()),
            unreachable: false,
        },
    ];
    // A host id is minted at random, so which of these two sorts first is not
    // the fixture's to say.
    named.sort_by(|left, right| left.id.cmp(&right.id));
    assert_eq!(
        hosts, &named,
        "a host with no rows here is still named, by the id its sessions are \
         namespaced under and not by anything this gateway made up, which is the \
         empty group a client renders instead of nothing: {hosts:?}",
    );
    assert!(
        rows.iter().any(|row| row.id == kept && !row.unreachable),
        "and the host that is there is served as usual: {rows:?}",
    );

    // It comes back on its own, and the group fills in.
    down.restart().await;
    let seen = decoded_until(&mut events, "the returned host's rows", |frame| {
        list_frame(frame).is_some_and(|(rows, _)| rows.iter().any(|row| row.id == lost))
    })
    .await;
    let (_, hosts) = seen
        .iter()
        .rev()
        .find_map(list_frame)
        .expect("a list frame with the returned host's rows");
    assert!(
        hosts.iter().all(|host| !host.unreachable),
        "the host answered again, so nothing is unreachable: {hosts:?}",
    );

    fixture.shutdown().await;
    down.stop().await;
    up.stop().await;
}

/// A host's name outlives the gateway process that learned it, so a host that is
/// down when the gateway comes back is still labelled by its name rather than
/// regressing to hex (spec 7.1). This is what the name is written down for.
///
/// One host of each enrollment kind, because the two learn a name on different
/// paths: a dynamic enrollment records what the enrolling handshake reported, a
/// configured host has nothing but its link's contact to learn one from.
///
/// The record is read before the restart, because with nothing in the file this
/// measures nothing: the names would come off the live hosts instead, which is
/// the case that needs no persistence at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_downed_hosts_name_survives_a_gateway_restart() {
    let mut configured = Upstream::named(Some("~/work/umber/aj")).await;
    let mut dynamic = Upstream::named(Some("~/workshop")).await;
    let configured_at = HostAddress::parse(&configured.address()).expect("an address");
    let dynamic_at = HostAddress::parse(&dynamic.address()).expect("an address");

    let fixture = Fixture::new(&[&configured]).await;
    fixture.until_connected(&configured.host_id()).await;
    assert_eq!(
        fixture.enroll(&dynamic.address()).await.status(),
        StatusCode::OK,
    );
    fixture.until_connected(&dynamic.host_id()).await;

    let record = recorded(&fixture);
    assert_eq!(
        record.configured_ids,
        vec![enrolled_naming(
            &configured_at,
            &configured.host_id(),
            "~/work/umber/aj",
        )],
        "a configured host's name is written down beside the id its link learned, \
         or there is nothing here for a restart to read: {record:?}",
    );
    assert_eq!(
        record.hosts,
        vec![enrolled_naming(
            &dynamic_at,
            &dynamic.host_id(),
            "~/workshop"
        )],
        "and so is the name the enrolling handshake reported: {record:?}",
    );

    configured.stop().await;
    dynamic.stop().await;
    let fixture = fixture.restart().await;

    let labelled = fixture
        .until("both hosts, unreachable, in the merged directory", |list| {
            (list.hosts.len() == 2 && list.hosts.iter().all(|host| host.unreachable))
                .then(|| list.hosts.clone())
        })
        .await;
    let mut named = vec![
        DirectoryHost {
            id: Some(configured.host_id()),
            address: None,
            name: Some("~/work/umber/aj".to_string()),
            unreachable: true,
        },
        DirectoryHost {
            id: Some(dynamic.host_id()),
            address: None,
            name: Some("~/workshop".to_string()),
            unreachable: true,
        },
    ];
    // A host id is minted at random, so which of these two sorts first is not
    // the fixture's to say.
    named.sort_by(|left, right| left.id.cmp(&right.id));
    assert_eq!(
        labelled, named,
        "a host this gateway cannot reach is still published under the name it \
         reported, off the record and not off a hello it cannot have: {labelled:?}",
    );

    fixture.shutdown().await;
}

/// A name follows the host that reports it: one that comes back calling itself
/// something else is relabelled, and the record follows, or the next restart
/// would bring the old label back.
///
/// Its id does not move with it, which is the whole distinction: the sessions
/// this gateway namespaced under that id are still that host's, and still
/// address the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_that_comes_back_under_a_new_name_is_relabelled() {
    let mut host = Upstream::named(Some("~/work/umber/aj")).await;
    let session = host.create().await;
    let at = HostAddress::parse(&host.address()).expect("an address");
    let fixture = Fixture::new(&[&host]).await;
    fixture.until_connected(&host.host_id()).await;
    let named = host.namespaced(&session);
    fixture.row(&named).await;

    let before = fixture
        .until("the name this host started under", |list| {
            list.hosts.first().map(|host| host.name.clone())
        })
        .await;
    assert_eq!(
        before,
        Some("~/work/umber/aj".to_string()),
        "the first name has to be published, or a relabelling is unobservable",
    );

    host.stop().await;
    host.restart_as(Some("the-builder")).await;

    // Reachable as well as renamed: a link settles the name it was told before
    // it reports the connection (see `gateway::link`), so waiting on the name
    // alone would race the row assertion below against one loopback request.
    let relabelled = fixture
        .until("the name the host came back under", |list| {
            list.hosts
                .iter()
                .find(|host| host.name.as_deref() == Some("the-builder") && !host.unreachable)
                .cloned()
        })
        .await;
    assert_eq!(
        relabelled.id,
        Some(host.host_id()),
        "the id is identity and does not move with the label: {relabelled:?}",
    );
    let row = fixture.row(&named).await;
    assert!(
        !row.unreachable,
        "and the session it namespaced is served under the id it always had: {row:?}",
    );
    let record = recorded(&fixture);
    assert_eq!(
        record.configured_ids,
        vec![enrolled_naming(&at, &host.host_id(), "the-builder")],
        "the record follows the latest contact, or the next restart would label \
         this host by a name it no longer answers with: {record:?}",
    );

    fixture.shutdown().await;
    host.stop().await;
}

/// A configured host this gateway has never spoken to is named by the address it
/// is enrolled at, and never by an id (spec 7.1).
///
/// An id namespaces sessions, so a synthetic one would poison every id a client
/// holds the moment the real one arrived, and a client that grouped rows under it
/// would have to re-key them all. An address is a label: the client renders the
/// empty group by it and addresses nothing with it.
///
/// Two hosts, one of them up, because a client cannot tell an empty directory
/// from one that has not arrived yet: the reachable host's row is what says the
/// merge ran at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_configured_host_that_never_answered_is_named_by_its_address() {
    let mut up = Upstream::start().await;
    let session = up.create().await;
    // Port 1, where nothing answers: the one host this gateway can never learn
    // an id from.
    let silent = HostAddress::parse("127.0.0.1:1").expect("an address");
    let fixture = Fixture::over(
        TempDir::new().expect("tempdir"),
        vec![
            HostAddress::parse(&up.address()).expect("an address"),
            silent.clone(),
        ],
    )
    .await;

    // The row first, because this measures nothing until the merge has run: an
    // empty payload would satisfy any claim about what is not in it.
    fixture.row(&up.namespaced(&session)).await;
    let list = fixture
        .client
        .sessions()
        .await
        .expect("the merged directory");

    assert_eq!(
        list.hosts,
        vec![
            DirectoryHost {
                id: None,
                address: Some(silent.to_string()),
                // A host that has never answered has said nothing about itself,
                // so there is no name to republish for it either.
                name: None,
                unreachable: true,
            },
            DirectoryHost {
                id: Some(up.host_id()),
                address: None,
                name: Some(up.host_name()),
                unreachable: false,
            },
        ],
        "the host that has never answered is named by its address with nothing \
         in the id position, and the one that has is named by the id its \
         sessions are namespaced under and by the name it reported: {:?}",
        list.hosts,
    );

    fixture.shutdown().await;
    up.stop().await;
}

/// The rows and the hosts of a `list` frame, `None` for every other frame.
fn list_frame(frame: &DecodedFrame) -> Option<(&Vec<SessionSummary>, &Vec<DirectoryHost>)> {
    match frame {
        DecodedFrame::Known(known) => match known.value() {
            Frame::List { sessions, hosts } => Some((sessions, hosts)),
            _ => None,
        },
        DecodedFrame::Unknown { .. } => None,
    }
}

/// Every frame as it arrived on the wire, which is where a field this build has
/// no type for is still visible.
fn raw_json(frames: &[DecodedFrame]) -> String {
    frames
        .iter()
        .map(|frame| serde_json::to_string(frame).expect("a frame re-serializes"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_hosts_sessions_join_the_directory_on_enrollment() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let session = left.create().await;
    let other = right.create().await;
    let fixture = Fixture::new(&[&left]).await;

    fixture.row(&left.namespaced(&session)).await;

    assert_eq!(
        fixture.enroll(&right.address()).await.status(),
        StatusCode::OK,
    );
    fixture.row(&right.namespaced(&other)).await;
    let ids = fixture
        .until("both rows", |list| {
            (list.sessions.len() == 2).then(|| {
                list.sessions
                    .iter()
                    .map(|row| row.id.clone())
                    .collect::<Vec<_>>()
            })
        })
        .await;
    assert!(
        ids.contains(&left.namespaced(&session)) && ids.contains(&right.namespaced(&other)),
        "one row per session, from both hosts: {ids:?}",
    );

    fixture.shutdown().await;
    left.stop().await;
    right.stop().await;
}

// ---------------------------------------------------------------------------
// Proxying (spec 7.1, and 6.10's forward-don't-filter)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_reaches_the_host_that_owns_the_session() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let session = left.create().await;
    let fixture = Fixture::new(&[&left, &right]).await;
    fixture.row(&left.namespaced(&session)).await;

    fixture
        .client
        .tree(&left.namespaced(&session))
        .await
        .expect("the owning host answers the read");

    // The same session id under the other host's namespace: that host holds no
    // such session, so its own 404 comes back rather than the owner's answer.
    let err = fixture
        .client
        .tree(&right.namespaced(&session))
        .await
        .expect_err("the other host does not hold that session");
    assert_eq!(err.status(), Some(StatusCode::NOT_FOUND), "got {err:?}");
    assert_eq!(
        err.code(),
        Some("unknown_session"),
        "the host's own code travels back unchanged",
    );

    fixture.shutdown().await;
    left.stop().await;
    right.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_runs_on_the_owning_host_alone() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let session = left.create().await;
    let idle = right.create().await;
    let fixture = Fixture::new(&[&left, &right]).await;
    fixture.row(&left.namespaced(&session)).await;
    // A created session already holds a few entries (its system prompt, its
    // seed settings), so what a turn moves is the position, not its presence.
    let before = left.durable_seq(&session).await;
    let elsewhere = right.durable_seq(&idle).await;

    fixture
        .client
        .command(&left.namespaced(&session), &prompt("go"))
        .await
        .expect("the prompt is accepted");

    // The turn lands on the host that owns the session, which its own directory
    // reports with no gateway in the way. A prompt routed to the other host
    // would leave this waiting until the deadline.
    bounded("the turn to become durable", async {
        while left.durable_seq(&session).await <= before {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert_eq!(
        right.durable_seq(&idle).await,
        elsewhere,
        "nothing was appended on the host that owns nothing here",
    );

    fixture.shutdown().await;
    left.stop().await;
    right.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_id_no_namespace_can_hold_is_a_404() {
    let mut host = Upstream::start().await;
    let session = host.create().await;
    let fixture = Fixture::new(&[&host]).await;
    fixture.row(&host.namespaced(&session)).await;

    for id in [
        // A bare host-local id: what a host answers to, and meaningless here.
        session.as_str(),
        // A host this gateway does not have.
        "0123456789abcdef:whatever",
        // Nothing before the separator.
        ":whatever",
        // Nothing after it.
        "0123456789abcdef:",
    ] {
        let err = fixture
            .client
            .tree(id)
            .await
            .expect_err("no session on this gateway");
        assert_eq!(
            err.status(),
            Some(StatusCode::NOT_FOUND),
            "{id:?} got {err:?}",
        );
        assert_eq!(err.code(), Some("unknown_session"), "{id:?}");
    }

    fixture.shutdown().await;
    host.stop().await;
}

/// An error body a host wrote crosses the proxy with its session named in this
/// gateway's own vocabulary, and nothing else touched (spec 6.6).
///
/// The host names the session as the host knows it, which is an id no client of
/// this gateway can address: the rewrite is the same one the create answer gets,
/// and everything around it travels as the host wrote it, the fields this build
/// has no name for included (spec 6.10).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_proxied_error_body_names_the_session_this_gateways_way() {
    let recorder = Recorder::start("recorder").await;
    let fixture = Fixture::over(
        TempDir::new().expect("tempdir"),
        vec![recorder.address.clone()],
    )
    .await;
    fixture.until_connected("recorder").await;

    let response = fixture
        .http
        .get(format!(
            "{}/v1/sessions/recorder:s-1/{REFUSED_ROUTE}",
            fixture.server.url()
        ))
        .send()
        .await
        .expect("the proxied request");

    let status = response.status();
    let body: serde_json::Value = response.json().await.expect("a JSON body");
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body["session"], "recorder:s-1",
        "the host's own id for it is one no client here can address: {body}",
    );
    assert_eq!(
        body["code"], "unknown_session",
        "and the host's own code travels: {body}",
    );
    assert_eq!(
        body["hint"], "look it up",
        "as does a field this gateway has no name for: {body}",
    );

    fixture.shutdown().await;
    recorder.stop();
}

// ---------------------------------------------------------------------------
// A host that is not there (spec 6.8's `unreachable`, 6.1's 503)
// ---------------------------------------------------------------------------

/// The enrolled-host entry `host` has in `list`.
///
/// What a client's group header renders its reachability from, and the only
/// such signal it has for a host it holds no rows for.
fn host_group<'a>(list: &'a SessionList, host: &str) -> &'a DirectoryHost {
    list.hosts
        .iter()
        .find(|entry| entry.id.as_deref() == Some(host))
        .unwrap_or_else(|| panic!("{host} is named among the hosts: {:?}", list.hosts))
}

/// A gateway keeps a downed host's rows, marks them, and marks that host's own
/// group entry along with them.
///
/// Both marks come out of one merge and are asserted on one payload, because a
/// client renders the rows from `SessionSummary::unreachable` and the header
/// above them from `DirectoryHost::unreachable`: a header disagreeing with the
/// rows beneath it is what a regression between the two looks like.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_downed_hosts_sessions_stay_listed_and_read_unreachable() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let session = left.create().await;
    let other = right.create().await;
    let fixture = Fixture::new(&[&left, &right]).await;
    let downed = left.namespaced(&session);
    let alive = right.namespaced(&other);
    fixture.row(&downed).await;
    fixture.row(&alive).await;

    left.stop().await;

    let marked = fixture
        .until("the downed host's row to be marked", |list| {
            list.sessions
                .iter()
                .any(|row| row.id == downed && row.unreachable)
                .then(|| list.clone())
        })
        .await;
    let row = marked
        .sessions
        .iter()
        .find(|row| row.id == downed)
        .expect("the row this waited for");
    assert_eq!(
        row.host.as_deref(),
        Some(left.host_id().as_str()),
        "the row keeps everything the host last said about it",
    );
    let neighbour = fixture.row(&alive).await;
    assert!(
        !neighbour.unreachable,
        "one host going away says nothing about the other",
    );
    assert!(
        host_group(&marked, &left.host_id()).unreachable,
        "the rows read unreachable under a header that reads reachable, so a \
         client renders the downed host as if it were there: {:?}",
        marked.hosts,
    );
    assert!(
        !host_group(&marked, &right.host_id()).unreachable,
        "one host going away marked the other's header too: {:?}",
        marked.hosts,
    );
    let reported = fixture
        .until_hosts("the downed host to be reported as such", |hosts| {
            hosts
                .hosts
                .iter()
                .find(|host| host.id.as_deref() == Some(left.host_id().as_str()) && !host.connected)
                .cloned()
        })
        .await;
    assert!(
        reported.error.is_some(),
        "an operator asking after a host is told why it is not there",
    );

    // And it comes back on its own: the link keeps dialing.
    left.restart().await;
    let cleared = fixture
        .until("the returned host's row to clear", |list| {
            list.sessions
                .iter()
                .any(|row| row.id == downed && !row.unreachable)
                .then(|| list.clone())
        })
        .await;
    assert!(
        !host_group(&cleared, &left.host_id()).unreachable,
        "the rows cleared under a header still marked unreachable: {:?}",
        cleared.hosts,
    );

    fixture.shutdown().await;
    left.stop().await;
    right.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_to_a_downed_host_answers_503() {
    let mut host = Upstream::start().await;
    let session = host.create().await;
    let fixture = Fixture::new(&[&host]).await;
    let id = host.namespaced(&session);
    fixture.row(&id).await;

    host.stop().await;
    fixture
        .until("the host to read unreachable", |list| {
            list.sessions
                .iter()
                .find(|row| row.id == id && row.unreachable)
                .cloned()
        })
        .await;

    let err = fixture
        .client
        .command(&id, &prompt("go"))
        .await
        .expect_err("a command cannot reach a host that is not there");
    assert_eq!(
        err.status(),
        Some(StatusCode::SERVICE_UNAVAILABLE),
        "got {err:?}",
    );
    assert_eq!(
        err.code(),
        Some("host_unreachable"),
        "the one status a gateway has that a host does not (spec 6.1)",
    );
    let err = fixture.client.tree(&id).await.expect_err("nor can a read");
    assert_eq!(err.status(), Some(StatusCode::SERVICE_UNAVAILABLE));

    fixture.shutdown().await;
}

// ---------------------------------------------------------------------------
// Enrollment (spec 7.1)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_enrollment_round_trips_and_outlives_the_process() {
    let mut host = Upstream::start().await;
    let session = host.create().await;
    let fixture = Fixture::new(&[]).await;
    assert!(
        fixture.hosts().await.hosts.is_empty(),
        "a gateway with no configuration and no enrollments has no hosts",
    );

    let enrolled: HostSummary = fixture
        .enroll(&host.address())
        .await
        .json()
        .await
        .expect("the enrolled host");
    assert_eq!(enrolled.id.as_deref(), Some(host.host_id().as_str()));
    assert_eq!(enrolled.source, HostSource::Dynamic);

    let listed = fixture.hosts().await;
    assert_eq!(listed.hosts.len(), 1);
    assert_eq!(listed.hosts[0].address, host.address());
    let id = host.namespaced(&session);
    fixture.row(&id).await;

    // A second gateway process over the same state directory recovers it.
    let fixture = fixture.restart().await;
    let recovered = fixture.hosts().await;
    assert_eq!(
        recovered.hosts.len(),
        1,
        "a dynamic enrollment survives a restart",
    );
    assert_eq!(
        recovered.hosts[0].id.as_deref(),
        Some(host.host_id().as_str()),
    );
    fixture.row(&id).await;

    assert_eq!(
        fixture.withdraw(&host.host_id()).await.status(),
        StatusCode::NO_CONTENT,
    );
    assert!(fixture.hosts().await.hosts.is_empty());
    fixture
        .until("the withdrawn host's rows to go", |list| {
            list.sessions.is_empty().then_some(())
        })
        .await;
    // The withdrawal is remembered too, not just applied.
    let fixture = fixture.restart().await;
    assert!(
        fixture.hosts().await.hosts.is_empty(),
        "a withdrawal outlives the process that served it",
    );

    fixture.shutdown().await;
    host.stop().await;
}

/// A configured host is the configuration file's to hold, in both directions:
/// the gateway will not withdraw one, and it does not keep one after the operator
/// removes it from the file. The id it learned for it goes too, since an id
/// records identity and never enrollment (spec 7.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_static_config_host_is_enrolled_and_cannot_be_withdrawn() {
    let mut host = Upstream::start().await;
    let session = host.create().await;
    let fixture = Fixture::new(&[&host]).await;

    let listed = fixture.hosts().await;
    assert_eq!(listed.hosts.len(), 1);
    assert_eq!(listed.hosts[0].source, HostSource::Config);
    fixture.row(&host.namespaced(&session)).await;

    let (status, code, message) = refusal(fixture.withdraw(&host.host_id()).await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code, "static_host");
    assert!(
        message.contains("configuration"),
        "the refusal points at what would bring it back: {message:?}",
    );
    assert_eq!(
        fixture.hosts().await.hosts.len(),
        1,
        "and it is still there"
    );

    // A static entry is the configuration's to hold, so the gateway records only
    // its id and never its existence: restarted over a file that no longer names
    // it, it is gone.
    let fixture = fixture.restarted_over(Vec::new()).await;
    assert!(
        fixture.hosts().await.hosts.is_empty(),
        "a static host comes back from the configuration or not at all",
    );
    let recorded =
        std::fs::read_to_string(fixture.state.path().join("hosts.json")).unwrap_or_default();
    assert!(
        !recorded.contains(&host.host_id()),
        "and the id learned for it does not sit in the state file waiting to \
         bring it back at the next restart: {recorded}",
    );

    fixture.shutdown().await;
    host.stop().await;
}

/// A learned id is written down the moment it is learned, not only when
/// something else happens to write the gateway's record (spec 7.1).
///
/// A gateway whose hosts all come from the configuration file never enrolls or
/// withdraws anything, which is exactly what the other two write paths are. An id
/// recorded only by those would never be recorded at all on such a gateway, and
/// every restart while one of its hosts was down would come back unable to name
/// that host. Nothing here enrolls or withdraws, so the only write that can
/// happen is the one under test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_learned_id_is_written_down_when_it_is_learned() {
    let mut host = Upstream::start().await;
    let fixture = Fixture::new(&[&host]).await;

    fixture.until_connected(&host.host_id()).await;

    let state = EnrollmentFile::new(fixture.state.path());
    let recorded = bounded("the learned id to be written down", async {
        loop {
            let recorded = state.load().expect("the gateway's own record");
            if !recorded.configured_ids.is_empty() {
                return recorded;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    assert_eq!(
        recorded.configured_ids[0].host_id,
        host.host_id(),
        "the id this gateway namespaces that host's sessions under is the one it \
         wrote down",
    );
    assert!(
        recorded.hosts.is_empty(),
        "and it is recorded as identity only: an entry among the enrollments \
         would make this file the record of a configured host's existence, and \
         resurrect one the operator removed from the configuration: {recorded:?}",
    );

    fixture.shutdown().await;
    host.stop().await;
}

/// A learned id is a cache for the next run, so a write that fails is a log line
/// and nothing more: the host has just answered, and refusing to serve it over a
/// cache write would trade a working host for a note.
///
/// The opposite of an enrollment, which is an operator's instruction and does not
/// stand unless it is recorded (see
/// [`an_enrollment_the_gateway_cannot_record_does_not_stand`]). The asymmetry
/// matters most for exactly the host here: a configured one cannot be
/// re-enrolled to get out of it.
///
/// The host is unreachable while the state file is staged, because its id would
/// otherwise be learned and written down before there was anything to stage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_learned_id_that_cannot_be_written_down_still_names_its_host() {
    let mut host = Upstream::start().await;
    let session = host.create().await;
    let bridge = Bridge::to(&host).await;
    bridge.cut();
    let fixture = Fixture::over(
        TempDir::new().expect("tempdir"),
        vec![bridge.address.clone()],
    )
    .await;
    // A directory where the state file goes: the rename that publishes it cannot
    // land, so the save fails for a reason no permission bit is needed to stage.
    let state = fixture.state.path().join("hosts.json");
    std::fs::create_dir(&state).expect("stage an unwritable state file");

    bridge.heal();

    fixture.until_connected(&host.host_id()).await;
    let row = fixture.row(&host.namespaced(&session)).await;
    assert!(
        !row.unreachable,
        "the host that answered is served under the id it reported: {row:?}",
    );
    assert!(
        state.is_dir(),
        "the record was written after all, so nothing here measures what this \
         gateway does when it cannot write one",
    );

    std::fs::remove_dir(&state).expect("unstage");
    fixture.shutdown().await;
    bridge.stop();
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_host_cannot_be_enrolled_twice() {
    let mut host = Upstream::start().await;
    let fixture = Fixture::new(&[&host]).await;
    fixture
        .until_hosts("the configured host to answer", |hosts| {
            hosts
                .hosts
                .iter()
                .find(|enrolled| enrolled.id.is_some())
                .cloned()
        })
        .await;

    // The same address, spelled the way a user would rather than the way the
    // configuration did.
    let bare = host.address().replace("http://", "");
    let (status, code, _) = refusal(fixture.enroll(&bare).await).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "that address is already there"
    );
    assert_eq!(code, "already_enrolled");

    // A second address onto the same store: one namespace, so one enrollment.
    let second = RemoteServer::bind(
        host.host.clone(),
        addr("127.0.0.1:0"),
        IdentityGate::local(),
    )
    .await
    .expect("a second port onto the same host");
    let (status, code, _) = refusal(fixture.enroll(&second.url()).await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        code, "duplicate_host",
        "two enrollments of one host id would collide on one namespace",
    );
    assert_eq!(fixture.hosts().await.hosts.len(), 1);

    second.shutdown().await;
    fixture.shutdown().await;
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enrolling_something_that_is_not_a_host_is_refused() {
    let fixture = Fixture::new(&[]).await;

    let (status, code, _) = refusal(fixture.enroll("not an address").await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code, "invalid_request");

    // Nothing answers there, and a gateway needs the host's id to namespace
    // it, so it refuses rather than enrolling something it cannot address.
    let (status, code, _) = refusal(fixture.enroll("127.0.0.1:1").await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code, "host_unreachable");
    assert!(fixture.hosts().await.hosts.is_empty());

    let (status, code, _) = refusal(fixture.withdraw("0123456789abcdef").await).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code, "unknown_host");

    fixture.shutdown().await;
}

/// A host reporting an id this gateway cannot namespace with is refused where it
/// arrives.
///
/// The grammar is checked at the boundary, exactly as a session id's is (spec
/// 6.2), and enrollment is a boundary: the id comes off the wire. Recording one
/// the rest of the module forbids would enroll a host that can never connect,
/// because adopting the id refuses it on every dial, and it would keep making
/// every create that names no host ambiguous, across restarts, reported as
/// nothing but a host that never answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_id_this_gateway_cannot_namespace_with_is_refused_at_enrollment() {
    let (url, serving) = canned_server(
        serde_json::json!({"protocol": PROTOCOL_VERSION, "capabilities": [],
                           "app_version": "0", "host_id": "with:colon"}),
        Vec::new(),
    )
    .await;
    let fixture = Fixture::new(&[]).await;

    let response = fixture.enroll(&url).await;

    // Read what the gateway kept before what it answered: an enrollment that
    // stuck is the lasting half of this, in the set a create resolves against
    // and in the file a restart reads.
    assert!(
        fixture.hosts().await.hosts.is_empty(),
        "an id this gateway can never namespace with was enrolled anyway",
    );
    let recorded =
        std::fs::read_to_string(fixture.state.path().join("hosts.json")).unwrap_or_default();
    assert!(
        !recorded.contains("with:colon"),
        "and written down, so it comes back after a restart: {recorded}",
    );
    let (status, code, _) = refusal(response).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code, "unusable_host_id");

    fixture.shutdown().await;
    serving.abort();
}

/// A remembered host id the grammar refuses is dropped when the file is read,
/// and the file stops naming it.
///
/// The state file is the gateway's own memory and a person can edit it, so it is
/// read with the same suspicion as the wire (spec 6.2): an id nothing can route
/// would otherwise sit in the enrolled set forever, connecting no host and
/// counting against every create that names none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remembered_host_id_the_grammar_refuses_is_dropped_at_startup() {
    let state = TempDir::new().expect("tempdir");
    std::fs::write(
        state.path().join("hosts.json"),
        r#"{"hosts":[{"address":"http://127.0.0.1:1","host_id":"with:colon"}]}"#,
    )
    .expect("a state file this gateway will read");

    let fixture = Fixture::over(state, Vec::new()).await;

    assert!(
        fixture.hosts().await.hosts.is_empty(),
        "an id this gateway can never namespace with came back from the file",
    );
    let recorded =
        std::fs::read_to_string(fixture.state.path().join("hosts.json")).expect("the state file");
    assert!(
        !recorded.contains("with:colon"),
        "and stayed in it, so it comes back at the next restart too: {recorded}",
    );

    fixture.shutdown().await;
}

/// An enrollment the gateway cannot write down does not stand, in either
/// direction: it would otherwise come back, or disappear, at the next restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_enrollment_the_gateway_cannot_record_does_not_stand() {
    let mut host = Upstream::start().await;
    let fixture = Fixture::new(&[]).await;
    let state = fixture.state.path().join("hosts.json");

    // A directory where the state file goes: the rename that publishes it cannot
    // land, so the save fails for a reason no permission bit is needed to stage.
    std::fs::create_dir(&state).expect("stage an unwritable state file");
    let (status, code, _) = refusal(fixture.enroll(&host.address()).await).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(code, "internal");
    assert!(
        fixture.hosts().await.hosts.is_empty(),
        "the enrollment was rolled back rather than left unrecorded",
    );

    // With the file writable the same enrollment sticks, and then the same
    // failure on the way out leaves it in place.
    std::fs::remove_dir(&state).expect("unstage");
    assert_eq!(
        fixture.enroll(&host.address()).await.status(),
        StatusCode::OK
    );
    std::fs::remove_file(&state).expect("unstage the file");
    std::fs::create_dir(&state).expect("stage it again");
    let (status, _, _) = refusal(fixture.withdraw(&host.host_id()).await).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        fixture.hosts().await.hosts.len(),
        1,
        "a withdrawal that was not recorded did not happen",
    );

    std::fs::remove_dir(&state).expect("unstage");
    fixture.shutdown().await;
    host.stop().await;
}

/// Two enrollments in flight at once both stick, in the directory and in the
/// file: the record of what was enrolled is written under the same lock as the
/// enrolling, so one cannot overwrite the other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_enrollments_at_once_are_both_recorded() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let fixture = Fixture::new(&[]).await;

    let (left_address, right_address) = (left.address(), right.address());
    let (first, second) = tokio::join!(
        fixture.enroll(&left_address),
        fixture.enroll(&right_address),
    );
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(fixture.hosts().await.hosts.len(), 2);

    let fixture = fixture.restart().await;
    let recovered = fixture.hosts().await;
    assert_eq!(
        recovered.hosts.len(),
        2,
        "both were written down, not just the one that finished last: {recovered:?}",
    );

    fixture.shutdown().await;
    left.stop().await;
    right.stop().await;
}

/// A remembered host the configuration now also names is one host, and the
/// configuration is the record of it being enrolled at all from then on
/// (spec 7.1).
///
/// Its id is not that record's to lose. An id names a store, and an operator
/// promoting a dynamic enrollment into the configuration file did not change
/// which store answers at that address, so the id carries across as the
/// configured host's. The host is down here, which is the case that makes the
/// difference: nothing will re-learn the id this run, so a client's group for it
/// is named by what the state file kept, or by an address.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remembered_host_the_configuration_names_too_keeps_its_id() {
    let state = TempDir::new().expect("tempdir");
    let address = HostAddress::parse("127.0.0.1:1").expect("an address");
    std::fs::write(
        state.path().join("hosts.json"),
        format!(r#"{{"hosts":[{{"address":"{address}","host_id":"remembered"}}]}}"#),
    )
    .expect("write the state");

    let fixture = Fixture::over(state, vec![address.clone()]).await;

    let hosts = fixture.hosts().await;
    assert_eq!(hosts.hosts.len(), 1, "one address is one host: {hosts:?}");
    assert_eq!(hosts.hosts[0].source, HostSource::Config);
    assert_eq!(
        hosts.hosts[0].id.as_deref(),
        Some("remembered"),
        "the id a host's sessions are namespaced under does not depend on which \
         file enrolled it: {hosts:?}",
    );
    assert_eq!(
        fixture
            .client
            .sessions()
            .await
            .expect("the merged directory")
            .hosts,
        vec![DirectoryHost {
            id: Some("remembered".to_string()),
            address: None,
            name: None,
            unreachable: true,
        }],
        "and a client's group for it is named by that id rather than by an \
         address it cannot address a session with",
    );
    // The record has moved rather than gone: the enrollment is the
    // configuration's, the id is this file's.
    let recorded = recorded(&fixture);
    assert_eq!(
        recorded.hosts,
        Vec::new(),
        "the remembered enrollment stayed, and would resurrect this host once the \
         operator removed it from the configuration",
    );
    assert_eq!(
        recorded.configured_ids,
        vec![enrolled_as(&address, "remembered")],
    );

    fixture.shutdown().await;
}

/// A configured enrollment names an address, so the operator's intent is
/// whatever aj host answers there and the id this gateway holds for it is
/// provisional (spec 7.1). Contact under a new id is a rebuilt host, and it runs
/// the whole sequence: the old identity is withdrawn, its group's attached
/// sessions are reset, its rows leave, and the state file adopts the new id.
///
/// Two hosts, because that teardown is a splice teardown: the client here is
/// attached across both, and the one that was not rebuilt has to keep its
/// session and its stream through it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_configured_hosts_contact_under_a_new_id_replaces_the_old_identity() {
    let rebuilt = FakeHost::rebuildable("before", "after", "s-2", Script::Blocks).await;
    let other = FakeHost::with_rows(
        "other",
        Script::Blocks,
        vec![serde_json::to_string(&fake_row("s-9")).expect("a row")],
    )
    .await;
    // Configured rather than enrolled over the wire: the address is what the
    // operator named, and the id at it is this gateway's to learn.
    let fixture = Fixture::over(
        TempDir::new().expect("tempdir"),
        vec![rebuilt.address.clone(), other.address.clone()],
    )
    .await;
    fixture.until_connected("before").await;
    fixture.until_connected("other").await;
    let mut events = fixture
        .attach(&[attach("before:s-1"), attach("other:s-9")])
        .await;
    let opened = carried_until(&mut events, "both attach blocks", |carried| {
        carried.caught_up.len() == 2
    })
    .await;
    assert_eq!(
        rebuilt.spliced_attaches().len(),
        1,
        "the upstream whose teardown this test is about was never opened: {opened:?}",
    );
    let restored = recorded(&fixture).configured_ids;
    assert!(
        restored.contains(&enrolled_as(&rebuilt.address, "before")),
        "the id a configured host answered to is recorded, or there is no \
         restored id here for contact to replace: {restored:?}",
    );

    rebuilt.rebuild();

    // The old identity is withdrawn in every sense that matters to the client
    // attached under it: the sessions it namespaced are reset, and the client's
    // stream and its other host are untouched.
    let torn_down = carried_until(
        &mut events,
        "the reset a replaced identity owes",
        |carried| !carried.resets.is_empty(),
    )
    .await;
    assert_eq!(
        torn_down.resets,
        vec!["before:s-1".to_string()],
        "the identity that was replaced reset something other than its own \
         sessions: {torn_down:?}",
    );
    assert!(
        !torn_down.ended,
        "the client's whole stream ended over one host being rebuilt: {torn_down:?}",
    );
    // The window a second `reset` would have arrived in, and the other host's own
    // stream. Both pumps would answer a teardown of both at once, in whichever
    // order the scheduler picked, so reading until the first reset says nothing
    // about the host that was not rebuilt.
    let others = frames_within(&mut events, QUIET).await;
    assert!(
        !resets(&others).contains(&"other:s-9".to_string()),
        "the host that was not rebuilt was asked to re-attach a session it never \
         lost: {others:?}",
    );
    assert_eq!(
        other.released(),
        0,
        "and its upstream went down with the identity that was replaced, which is \
         the collateral a client attached across hosts cannot afford",
    );

    // And the fresh contact behind it: the new identity is the namespace now,
    // and the rows under it are the rebuilt store's own.
    fixture.until_connected("after").await;
    assert!(!fixture.row("after:s-2").await.unreachable);
    let directory = fixture
        .client
        .sessions()
        .await
        .expect("the merged directory");
    assert_eq!(
        directory
            .hosts
            .iter()
            .filter_map(|host| host.id.clone())
            .collect::<Vec<_>>(),
        vec!["after".to_string(), "other".to_string()],
        "the group of the store that is gone is still there: {directory:?}",
    );
    // The rows a client can see now that the rebuilt host has published its own
    // directory. That `list` arrives moments after the identity is adopted and
    // replaces a stale row either way, so what the adoption itself drops is
    // pinned where that ordering cannot hide it, in `Directory::adopt`'s own
    // test.
    assert_eq!(
        directory
            .sessions
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        vec!["other:s-9", "after:s-2"],
        "the rebuilt store's own row and the other host's, and nothing else: a \
         row left over from the store that is gone names a session no id here \
         holds: {directory:?}",
    );
    let adopted = recorded(&fixture).configured_ids;
    assert!(
        adopted.contains(&enrolled_as(&rebuilt.address, "after"))
            && !adopted.iter().any(|host| host.host_id == "before"),
        "the state file is the record of an id that is no longer served, so the \
         next restart would namespace this host under a store that is gone: \
         {adopted:?}",
    );

    // The re-attach the reset asks for: refused for the id the old identity
    // namespaced, served for the one the new identity does.
    let mut resumed = fixture
        .client
        .events(&[attach("before:s-1"), attach("after:s-2")])
        .await
        .expect("a client stream onto the gateway");
    let served = frames_until(&mut resumed, "the block for the new identity", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    assert_eq!(
        refused_sessions(served.iter()),
        vec!["before:s-1"],
        "the id the store that is gone was namespaced under is the one refused: \
         {served:?}",
    );
    assert_eq!(
        named_sessions(&served)
            .into_iter()
            .filter(|session| session.starts_with("after:"))
            .collect::<Vec<_>>(),
        vec!["after:s-2", "after:s-2"],
        "and what the rebuilt host does hold is served under its new namespace: \
         {served:?}",
    );

    fixture.shutdown().await;
    rebuilt.stop();
    other.stop();
}

/// A dynamic enrollment names a host this gateway shook hands with, so its
/// recorded id is the record's referent: a different id at that address is a
/// store this enrollment is not about, and contact under one is refused
/// (spec 7.1).
///
/// The refusal has to name the remedy that works for a dynamic enrollment, which
/// is withdrawing it and enrolling the address again, so this carries that remedy
/// out: a message naming one that answers 409 is worse than no message at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dynamic_hosts_contact_under_a_new_id_is_refused() {
    let rebuilt = FakeHost::rebuildable("before", "after", "s-2", Script::Blocks).await;
    let fixture = Fixture::new(&[]).await;
    assert_eq!(
        fixture.enroll(&rebuilt.address.to_string()).await.status(),
        StatusCode::OK,
    );
    fixture.until_connected("before").await;

    rebuilt.rebuild();

    // The link keeps dialing and keeps being refused, and what it records is
    // what the operator reads.
    let refusal = fixture
        .until_hosts("the refusal of the id at that address", |hosts| {
            hosts
                .hosts
                .iter()
                .find(|host| host.id.as_deref() == Some("before"))
                .and_then(|host| host.error.clone())
                .filter(|error| error.contains("after"))
        })
        .await;
    assert!(
        refusal.contains("withdraw")
            && refusal.contains("enroll")
            && refusal.contains(&rebuilt.address.to_string()),
        "the refusal has to name the remedy that actually works here, which is \
         withdrawing this enrollment and enrolling the address again: {refusal}",
    );
    let hosts = fixture.hosts().await;
    assert_eq!(
        (hosts.hosts[0].id.as_deref(), hosts.hosts[0].connected),
        (Some("before"), false),
        "the enrollment keeps the id it is the record of, and says it cannot be \
         reached: {hosts:?}",
    );
    assert_eq!(
        recorded(&fixture).hosts,
        vec![enrolled_as(&rebuilt.address, "before")],
        "and nothing was adopted, so the record still names the host this \
         enrollment was made from",
    );

    // The remedy, carried out.
    assert_eq!(
        fixture.withdraw("before").await.status(),
        StatusCode::NO_CONTENT,
    );
    let enrolled = fixture.enroll(&rebuilt.address.to_string()).await;
    assert_eq!(enrolled.status(), StatusCode::OK);
    let summary: HostSummary = enrolled.json().await.expect("a host summary");
    assert_eq!(
        summary.id.as_deref(),
        Some("after"),
        "the enrollment the refusal asked for is the record of the host that is \
         there now",
    );
    fixture.until_connected("after").await;
    assert!(!fixture.row("after:s-2").await.unreachable);

    fixture.shutdown().await;
    rebuilt.stop();
}

/// A configured host's id is provisional however this gateway came by it, a
/// restored one included: it is still only the id that answered last time
/// (spec 7.1).
///
/// This is the case a persisted id introduced. The host at a configured address
/// is rebuilt while the gateway is down, so the id the state file restores names
/// a store that no longer exists, and the host's first contact is under a new
/// one. Refusing that contact leaves the group unreachable under a dead id
/// forever, with no remedy an operator can reach: a configured enrollment cannot
/// be withdrawn, and the address is the one the operator asked for.
///
/// The host is unreachable to begin with, which is what makes the restored id
/// observable at all: it is what this gateway namespaces by until something
/// answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_id_the_host_no_longer_answers_to_is_replaced_on_contact() {
    let mut host = Upstream::start().await;
    let session = host.create().await;
    let bridge = Bridge::to(&host).await;
    bridge.cut();
    let state = TempDir::new().expect("tempdir");
    std::fs::write(
        state.path().join("hosts.json"),
        format!(
            r#"{{"configured_ids":[{{"address":"{}","host_id":"before"}}]}}"#,
            bridge.address
        ),
    )
    .expect("write the state");

    let fixture = Fixture::over(state, vec![bridge.address.clone()]).await;

    // The id the file restored is what this gateway namespaces by until the host
    // answers, or the replacement below is about nothing.
    let restored = fixture
        .until_hosts("a dial of the host that is not there", |hosts| {
            hosts
                .hosts
                .first()
                .filter(|host| host.error.is_some())
                .map(|host| (host.id.clone(), host.connected, hosts.hosts.len()))
        })
        .await;
    assert_eq!(
        restored,
        (Some("before".to_string()), false, 1),
        "one configured host, named by the id the state file kept, and not there",
    );

    bridge.heal();

    // Contact, under the id the store that is there now answers to.
    fixture.until_connected(&host.host_id()).await;
    let row = fixture.row(&host.namespaced(&session)).await;
    assert!(!row.unreachable, "{row:?}");
    let hosts = fixture.hosts().await;
    assert_eq!(
        hosts.hosts[0].id.as_deref(),
        Some(host.host_id().as_str()),
        "the host the operator configured is reachable under the id it answers \
         to: {hosts:?}",
    );
    let adopted = recorded(&fixture).configured_ids;
    assert_eq!(
        adopted,
        vec![enrolled_naming(
            &bridge.address,
            &host.host_id(),
            &host.host_name()
        )],
        "and the file records that id, or the next start would restore a store \
         that is gone all over again: {adopted:?}",
    );

    fixture.shutdown().await;
    bridge.stop();
    host.stop().await;
}

/// What the gateway's state file records, read back as the gateway wrote it.
fn recorded(fixture: &Fixture) -> crate::gateway::enrollment::Recorded {
    let text =
        std::fs::read_to_string(fixture.state.path().join("hosts.json")).expect("the state file");
    serde_json::from_str(&text).expect("readable gateway state")
}

/// One entry of that record, for a host that reports no name for itself (every
/// [`FakeHost`], whose hello carries none).
fn enrolled_as(address: &HostAddress, host_id: &str) -> crate::gateway::enrollment::EnrolledHost {
    crate::gateway::enrollment::EnrolledHost {
        address: address.clone(),
        host_id: host_id.to_string(),
        name: None,
    }
}

/// The same for a host that names itself, which every real one does (spec 6.1).
fn enrolled_naming(
    address: &HostAddress,
    host_id: &str,
    name: &str,
) -> crate::gateway::enrollment::EnrolledHost {
    crate::gateway::enrollment::EnrolledHost {
        name: Some(name.to_string()),
        ..enrolled_as(address, host_id)
    }
}

// ---------------------------------------------------------------------------
// The gateway's own surface (spec 6.1, 6.3)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_names_the_gateway_and_omits_a_working_directory() {
    let fixture = Fixture::new(&[]).await;

    let hello = fixture.client.hello().await.expect("hello");

    assert_eq!(hello.protocol, PROTOCOL_VERSION);
    assert_eq!(hello.app_version, env!("CARGO_PKG_VERSION"));
    assert!(!hello.host_id.is_empty(), "a gateway names itself too");
    assert_eq!(
        hello.working_directory, None,
        "a gateway serves no working directory of its own (spec 6.1)",
    );
    assert_eq!(
        hello.name, None,
        "and it names the hosts behind it rather than itself: there is no group \
         header for a gateway to label, and a client that reached one addressed \
         it directly (spec 7.1)",
    );
    let id = hello.host_id;

    let fixture = fixture.restart().await;
    assert_eq!(
        fixture.client.hello().await.expect("hello").host_id,
        id,
        "its identity outlives the process",
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_that_attaches_nothing_carries_the_merged_directory() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let session = left.create().await;
    let fixture = Fixture::new(&[&left, &right]).await;
    fixture.row(&left.namespaced(&session)).await;

    let mut events = fixture
        .client
        .events(&[])
        .await
        .expect("a control stream onto the gateway");

    // The directory as it stands opens the stream: a client that attaches
    // nothing still has to learn what is there.
    let rows = bounded("a list frame", async {
        loop {
            match events.recv().await.expect("a frame").expect("a good frame") {
                Frame::List { sessions, .. } => return sessions,
                Frame::Heartbeat => continue,
                other => panic!("a gateway stream carries no {other:?} in this stage"),
            }
        }
    })
    .await;
    assert!(
        rows.iter().any(|row| row.id == left.namespaced(&session)),
        "the merged directory, namespaced: {rows:?}",
    );

    // A change upstream reaches the stream without the client asking again.
    let fresh = right.create().await;
    let rows = bounded("the directory after a create", async {
        loop {
            match events.recv().await.expect("a frame").expect("a good frame") {
                Frame::List { sessions, .. }
                    if sessions
                        .iter()
                        .any(|row| row.id == right.namespaced(&fresh)) =>
                {
                    return sessions;
                }
                _ => continue,
            }
        }
    })
    .await;
    assert!(rows.len() >= 2, "both hosts' rows: {rows:?}");

    fixture.shutdown().await;
    left.stop().await;
    right.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_gateway_stream_heartbeats() {
    let state = TempDir::new().expect("tempdir");
    let gateway = Gateway::new(GatewaySetup {
        state_dir: state.path().to_path_buf(),
        static_hosts: Vec::new(),
        tuning: Tuning {
            heartbeat: Duration::from_millis(50),
            ..tuning()
        },
    })
    .expect("a gateway");
    let server = GatewayServer::bind(gateway.clone(), addr("127.0.0.1:0"), IdentityGate::local())
        .await
        .expect("bind");
    let client = RemoteClient::new(&server.url()).expect("client");

    let mut events = client.events(&[]).await.expect("a stream");
    let mut heartbeats = 0;
    bounded("two heartbeats", async {
        while heartbeats < 2 {
            match events.recv().await.expect("a frame").expect("a good frame") {
                Frame::Heartbeat => heartbeats += 1,
                Frame::List { sessions, .. } => assert!(sessions.is_empty(), "{sessions:?}"),
                other => panic!("unexpected {other:?}"),
            }
        }
    })
    .await;

    drop(events);
    server.shutdown().await;
    gateway.shutdown().await;
}

/// A shutdown ends its clients' streams rather than waiting for them to leave.
///
/// A host's streams end because the host closes its attachments. A gateway has
/// nothing that would, so without ending them on shutdown every teardown with a
/// perfectly healthy client attached would sit out the server's whole grace
/// period.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shutdown_does_not_wait_out_an_attached_client() {
    let fixture = Fixture::new(&[]).await;
    let mut events = fixture.client.events(&[]).await.expect("a stream");
    bounded("the opening directory", async {
        events.recv().await.expect("a frame").expect("a good frame")
    })
    .await;

    let started = std::time::Instant::now();
    fixture.shutdown().await;
    let took = started.elapsed();

    assert!(
        took < Duration::from_secs(2),
        "the shutdown waited {took:?} on a client it could have closed",
    );
    // And the client is told, rather than left holding a stream nothing writes to.
    assert!(
        bounded("the end of the stream", events.recv())
            .await
            .is_none(),
        "the stream ended",
    );
}

/// A settled gateway stops talking: its link stays up rather than reconnecting,
/// and a directory that has not changed is not republished, because `list` is
/// cumulative and an identical snapshot carries no information (spec 6.8).
///
/// The host has to resend its directory for that to be about anything, so this
/// one does, on cue and twice: the same rows, then rows that differ. The second
/// is what proves the first was read and merged, because they arrive in order on
/// one connection, and it is what a quiet window on its own could never say.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unchanged_directory_publishes_nothing() {
    let settled = vec![serde_json::to_string(&fake_row("s-1")).expect("a row")];
    let changed = vec![
        serde_json::to_string(&fake_row("s-1")).expect("a row"),
        serde_json::to_string(&fake_row("s-2")).expect("a row"),
    ];
    let fake = FakeHost::republishing(
        "fake",
        settled.clone(),
        vec![settled.clone(), changed.clone()],
    )
    .await;
    let fixture = Fixture::over(TempDir::new().expect("tempdir"), vec![fake.address.clone()]).await;
    fixture.row("fake:s-1").await;

    let mut events = fixture.client.events(&[]).await.expect("a stream");
    let first = frames_until(&mut events, "the opening directory", |frame| {
        matches!(frame, Frame::List { .. })
    })
    .await;

    fake.republish(0);

    let quiet = frames_within(&mut events, QUIET).await;
    let republished: Vec<&Frame> = quiet
        .iter()
        .filter(|frame| matches!(frame, Frame::List { .. }))
        .collect();
    assert!(
        republished.is_empty(),
        "the host resent the directory it had already sent, and this gateway made \
         a frame of it for every client watching: {republished:?} after {first:?}",
    );

    // The directory that did change, which is what says the identical one before
    // it reached this gateway at all: they travel in order on one connection, so
    // this one arriving means that one was read and merged.
    fake.republish(1);
    let after = frames_until(
        &mut events,
        "the directory that changed",
        |frame| matches!(frame, Frame::List { sessions, .. } if sessions.len() == 2),
    )
    .await;
    assert_eq!(
        after
            .iter()
            .filter(|frame| matches!(frame, Frame::List { .. }))
            .count(),
        1,
        "one frame for the one snapshot that differs: {after:?}",
    );
    assert_eq!(
        fake.attaches().len(),
        1,
        "and one control connection throughout, because a link that reconnected \
         would republish the directory by itself: {:?}",
        fake.attaches(),
    );

    fixture.shutdown().await;
    fake.stop();
}

// ---------------------------------------------------------------------------
// Creating a session (spec 6.6)
// ---------------------------------------------------------------------------

/// One enrolled host needs no naming: a create that names none defaults to it.
///
/// The id that comes back is this gateway's vocabulary, which a read on it
/// proves rather than its spelling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_defaults_to_the_one_enrolled_host() {
    let mut host = Upstream::start().await;
    let fixture = Fixture::new(&[&host]).await;
    fixture.until_connected(&host.host_id()).await;

    let created = fixture
        .client
        .create_session(CreateSessionRequest::default())
        .await
        .expect("a create on the one host enrolled here");

    let address = SessionAddress::parse(&created.id)
        .unwrap_or_else(|err| panic!("a namespaced id, got {:?}: {err}", created.id));
    assert_eq!(
        host.session_ids().await,
        vec![address.session.clone()],
        "the create minted its session on the enrolled host",
    );
    assert_eq!(
        address.host,
        host.host_id(),
        "and the id names that host, which is what a client groups the row by",
    );
    fixture
        .client
        .tree(&created.id)
        .await
        .expect("the id a create answers with addresses the session");

    fixture.shutdown().await;
    host.stop().await;
}

/// Two enrolled hosts and a create naming neither is ambiguous, and ambiguity
/// is refused with a clear error rather than guessed at.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_naming_no_host_is_refused_when_two_are_enrolled() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let fixture = Fixture::new(&[&left, &right]).await;
    fixture.until_connected(&left.host_id()).await;
    fixture.until_connected(&right.host_id()).await;

    let response = fixture.create("{}").await;

    // The stores are where a guess would land, so they are read before the
    // response: whichever host was picked would hold a session nobody asked
    // for.
    let status = response.status();
    assert_eq!(
        (left.session_ids().await, right.session_ids().await),
        (Vec::new(), Vec::new()),
        "a host was guessed at and minted a session (answered {status})",
    );

    let (_, code, message) = refusal(response).await;
    assert_eq!(status, StatusCode::CONFLICT, "code {code}");
    assert_eq!(code, "ambiguous_host");
    assert!(
        message.contains(&left.host_id()) && message.contains(&right.host_id()),
        "the refusal names the hosts to choose between: {message}",
    );

    fixture.shutdown().await;
    left.stop().await;
    right.stop().await;
}

/// A create that names one of several hosts lands there and nowhere else, and
/// the id it answers with resolves as an address on this gateway.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_lands_on_the_host_it_names() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let fixture = Fixture::new(&[&left, &right]).await;
    fixture.until_connected(&left.host_id()).await;
    fixture.until_connected(&right.host_id()).await;

    let created = fixture
        .client
        .create_session(CreateSessionRequest {
            host: Some(right.host_id()),
            ..CreateSessionRequest::default()
        })
        .await
        .expect("a create on the host it names");

    let address = SessionAddress::parse(&created.id)
        .unwrap_or_else(|err| panic!("a namespaced id, got {:?}: {err}", created.id));
    assert_eq!(
        right.session_ids().await,
        vec![address.session.clone()],
        "the named host minted the session",
    );
    assert!(
        left.session_ids().await.is_empty(),
        "and the host that was not named was left alone",
    );
    assert_eq!(address.host, right.host_id());

    // The id resolves, which a turn on it proves: the prompt is accepted here
    // and becomes durable there. A returned id in the wrong vocabulary would
    // not route at all.
    let before = right.durable_seq(&address.session).await;
    fixture
        .client
        .command(&created.id, &prompt("go"))
        .await
        .expect("the id a create answers with addresses the session");
    bounded("the turn to become durable on the named host", async {
        while right.durable_seq(&address.session).await <= before {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;

    fixture.shutdown().await;
    left.stop().await;
    right.stop().await;
}

/// A named target that is not there is the same 503 a proxied command to it
/// answers (spec 6.1): a create is not held for a host that may come back, and
/// it certainly does not land somewhere else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_naming_a_host_that_is_not_there_answers_503() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let fixture = Fixture::new(&[&left, &right]).await;
    fixture.until_connected(&left.host_id()).await;
    fixture.until_connected(&right.host_id()).await;
    let downed = left.host_id();
    left.stop().await;
    fixture
        .until_hosts("the downed host to be marked", |hosts| {
            hosts
                .hosts
                .iter()
                .find(|host| host.id.as_deref() == Some(&downed) && !host.connected)
                .map(|_| ())
        })
        .await;

    let response = fixture.create(&format!(r#"{{"host":"{downed}"}}"#)).await;

    let status = response.status();
    assert!(
        right.session_ids().await.is_empty(),
        "the create landed on the host that was not named (answered {status})",
    );
    let (_, code, _) = refusal(response).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "code {code}");
    assert_eq!(code, "host_unreachable");

    fixture.shutdown().await;
    right.stop().await;
}

/// A gateway with nothing enrolled has nowhere to create a session, and says
/// so rather than answering as if it had tried.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_create_is_refused_when_nothing_is_enrolled() {
    let fixture = Fixture::new(&[]).await;

    let (status, code, message) = refusal(fixture.create("{}").await).await;

    assert_eq!(status, StatusCode::CONFLICT, "code {code}");
    assert_eq!(code, "no_host_enrolled");
    assert!(
        message.contains("enrolled"),
        "the refusal says what is missing: {message}",
    );

    fixture.shutdown().await;
}

/// A name no enrollment answers to names no host, and the create goes nowhere
/// else for it: a mistyped target must not land a session on whichever host
/// this gateway happens to hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_naming_a_host_that_is_not_enrolled_is_a_404() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let fixture = Fixture::new(&[&left, &right]).await;
    fixture.until_connected(&left.host_id()).await;
    fixture.until_connected(&right.host_id()).await;

    let response = fixture.create(r#"{"host":"0123456789abcdef"}"#).await;

    let status = response.status();
    assert_eq!(
        (left.session_ids().await, right.session_ids().await),
        (Vec::new(), Vec::new()),
        "the create fell back to an enrolled host (answered {status})",
    );
    let (_, code, _) = refusal(response).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "code {code}");
    assert_eq!(code, "unknown_host");

    fixture.shutdown().await;
    left.stop().await;
    right.stop().await;
}

/// What goes upstream is the client's own create body with one field changed:
/// `host`, set to the id of the host that answers it, because the create names
/// its target in that host's own vocabulary (spec 6.6).
///
/// A real host accepts an absent field as readily as its own id, so only a
/// server that keeps what it was sent can show the difference. The same
/// recording shows that everything else travels untouched, a field this build
/// does not know included (spec 6.10).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_forwarded_create_names_the_target_in_its_own_vocabulary() {
    let recorder = Recorder::start("recorder").await;
    let fixture = Fixture::over(
        TempDir::new().expect("tempdir"),
        vec![recorder.address.clone()],
    )
    .await;
    fixture.until_connected("recorder").await;

    let response = fixture
        .create_with_query(
            "newer=1",
            r#"{"tag":"fix-auth","added_later":{"n":18446744073709551616}}"#,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        recorder.create_queries(),
        vec![Some("newer=1".to_string())],
        "a parameter this build does not know is not this gateway's to drop",
    );
    let created: SessionCreated = response.json().await.expect("a created body");
    let sent = recorder.recorded();
    let body: serde_json::Value =
        serde_json::from_str(&sent).expect("the forwarded create is JSON");
    assert_eq!(
        body["host"],
        serde_json::json!("recorder"),
        "the create names the host that answers it: {sent}",
    );
    assert_eq!(
        body["tag"],
        serde_json::json!("fix-auth"),
        "and carries the rest of the client's body: {sent}",
    );
    assert!(
        sent.contains("18446744073709551616"),
        "a number no float holds travels as it was written: {sent}",
    );
    assert_eq!(
        created.id, "recorder:recorded-1",
        "and the answer namespaces the id the host minted",
    );

    fixture.shutdown().await;
    recorder.stop();
}

/// A create for a host whose control connection is down is refused even though
/// that host's port would answer it: what a client is told about a host (an
/// unreachable row) and what a create on it does have to agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_is_refused_for_a_host_the_gateway_has_no_link_to() {
    let recorder = Recorder::unlinked("recorder").await;
    let fixture = Fixture::over(
        TempDir::new().expect("tempdir"),
        vec![recorder.address.clone()],
    )
    .await;
    // It answers `hello`, so the gateway learns its id, and its stream never
    // opens, so the gateway never has a connection to it.
    fixture
        .until_hosts("the host to be named and not connected", |hosts| {
            hosts
                .hosts
                .iter()
                .find(|host| host.id.as_deref() == Some("recorder") && !host.connected)
                .map(|_| ())
        })
        .await;

    let response = fixture.create(r#"{"host":"recorder"}"#).await;

    let status = response.status();
    assert!(
        recorder.creates().is_empty(),
        "a create reached a host this gateway has no link to (answered {status})",
    );
    let (_, code, _) = refusal(response).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "code {code}");
    assert_eq!(code, "host_unreachable");

    fixture.shutdown().await;
    recorder.stop();
}

/// A create the host itself refuses comes back as the host's own refusal.
///
/// The gateway reads the body for its `host` field and judges nothing else, so
/// a thinking level only the host has a vocabulary for is the host's call to
/// make (spec 6.10, and spec 8's strictness about stated settings).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hosts_own_create_refusal_travels_back() {
    let mut host = Upstream::start().await;
    let fixture = Fixture::new(&[&host]).await;
    fixture.until_connected(&host.host_id()).await;

    let response = fixture
        .create(r#"{"settings":{"thinking":"ludicrous"}}"#)
        .await;

    let status = response.status();
    assert!(
        host.session_ids().await.is_empty(),
        "a refused create minted nothing (answered {status})",
    );
    let (_, code, message) = refusal(response).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "code {code}");
    assert_eq!(
        code, "invalid_request",
        "the host's own code, not this gateway's reading of the body",
    );
    assert!(
        message.contains("ludicrous"),
        "and the host's own words: {message}",
    );

    fixture.shutdown().await;
    host.stop().await;
}

/// A create body a host refuses is refused through a gateway too (spec 6.10,
/// 7.1).
///
/// A gateway edits the body for the one field it owns and normalizes nothing
/// else, so a duplicate key reaches the host that judges it. Collapsing the two
/// occurrences would launder a body the host refuses into one it accepts, which
/// is a client getting a different answer for having a gateway in the path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_body_that_repeats_a_key_is_the_hosts_to_refuse() {
    let mut host = Upstream::start().await;
    let fixture = Fixture::new(&[&host]).await;
    fixture.until_connected(&host.host_id()).await;
    let body = format!(r#"{{"host":"{id}","host":"{id}"}}"#, id = host.host_id());

    let direct = fixture
        .http
        .post(format!("{}/v1/sessions", host.address()))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.clone())
        .send()
        .await
        .expect("the create request");
    let refused = direct.status();
    assert_eq!(
        refused,
        StatusCode::BAD_REQUEST,
        "the host refuses this body, and this measures nothing unless it does",
    );

    let response = fixture.create(&body).await;

    let status = response.status();
    assert!(
        host.session_ids().await.is_empty(),
        "the gateway created a session from a body this host refuses (answered {status})",
    );
    assert_eq!(
        status, refused,
        "the same body: {refused} against the host, {status} through the gateway",
    );

    fixture.shutdown().await;
    host.stop().await;
}

/// The one route a [`Recorder`] refuses, so a test can watch an error body cross
/// the proxy.
const REFUSED_ROUTE: &str = "refuse";

/// A stand-in host that keeps the create bodies it is sent.
struct Recorder {
    address: HostAddress,
    creates: Arc<StdMutex<Vec<String>>>,
    /// The query string of every create it was sent, `None` for one that carried
    /// none.
    create_queries: Arc<StdMutex<Vec<Option<String>>>>,
    proxied: Arc<StdMutex<Vec<String>>>,
    serving: tokio::task::JoinHandle<()>,
}

impl Recorder {
    /// A recorder the gateway can link to.
    async fn start(host_id: &str) -> Self {
        Self::serve(host_id, true).await
    }

    /// A recorder that answers everything except an event stream, so the
    /// gateway learns its id from `hello` and never has a control connection to
    /// it. Its create route works perfectly well, which is what makes "the
    /// gateway will not use a host it is not linked to" observable.
    async fn unlinked(host_id: &str) -> Self {
        Self::serve(host_id, false).await
    }

    async fn serve(host_id: &str, stream: bool) -> Self {
        use axum::response::sse::{Event, Sse};
        use axum::routing::{get, post};

        let creates: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let create_queries: Arc<StdMutex<Vec<Option<String>>>> =
            Arc::new(StdMutex::new(Vec::new()));
        let proxied: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let hello = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "capabilities": [],
            "app_version": "0",
            "host_id": host_id,
        });
        let mut app = axum::Router::new().route(
            "/v1/hello",
            get(move || {
                let hello = hello.clone();
                async move { axum::Json(hello) }
            }),
        );
        if stream {
            app = app.route(
                "/v1/events",
                get(|| async {
                    // One frame and then silence: the frame gets the response
                    // head out, and the stream staying open is what keeps the
                    // gateway's link connected. A stream that ended would mark
                    // this host unreachable, and a create would then be refused
                    // before it was ever forwarded.
                    let opening = futures::stream::iter([Ok::<_, std::convert::Infallible>(
                        Event::default().data(r#"{"kind":"heartbeat"}"#),
                    )]);
                    Sse::new(futures::StreamExt::chain(
                        opening,
                        futures::stream::pending(),
                    ))
                }),
            );
        }
        let app = app
            .route(
                "/v1/sessions",
                post({
                    let creates = Arc::clone(&creates);
                    let create_queries = Arc::clone(&create_queries);
                    move |uri: axum::http::Uri, body: String| {
                        let creates = Arc::clone(&creates);
                        let create_queries = Arc::clone(&create_queries);
                        async move {
                            create_queries
                                .lock()
                                .expect("the queries mutex is poisoned")
                                .push(uri.query().map(str::to_string));
                            let mut held = creates.lock().expect("the creates mutex is poisoned");
                            held.push(body);
                            axum::Json(
                                serde_json::json!({"id": format!("recorded-{}", held.len())}),
                            )
                        }
                    }
                }),
            )
            .route(
                "/v1/sessions/{id}/{*rest}",
                axum::routing::any({
                    let proxied = Arc::clone(&proxied);
                    move |path: axum::extract::Path<(String, String)>| {
                        let proxied = Arc::clone(&proxied);
                        async move {
                            use axum::response::IntoResponse;

                            let axum::extract::Path((id, rest)) = path;
                            proxied
                                .lock()
                                .expect("the proxied mutex is poisoned")
                                .push(format!("{id}/{rest}"));
                            // One route refuses, in the shape spec 6.6 gives an
                            // error about a session: the id in the host's own
                            // vocabulary, plus a field of its own.
                            if rest == REFUSED_ROUTE {
                                return (
                                    StatusCode::NOT_FOUND,
                                    axum::Json(serde_json::json!({
                                        "code": "unknown_session",
                                        "message": format!("no session {id} here"),
                                        "session": id,
                                        "hint": "look it up",
                                    })),
                                )
                                    .into_response();
                            }
                            axum::Json(serde_json::json!({})).into_response()
                        }
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
        Self {
            address: HostAddress::parse(&format!("http://{bound}")).expect("an address"),
            creates,
            create_queries,
            proxied,
            serving,
        }
    }

    /// Every proxied session route this recorder was sent, as `id/rest`.
    fn proxied(&self) -> Vec<String> {
        self.proxied
            .lock()
            .expect("the proxied mutex is poisoned")
            .clone()
    }

    /// The query string of every create this recorder was sent.
    fn create_queries(&self) -> Vec<Option<String>> {
        self.create_queries
            .lock()
            .expect("the queries mutex is poisoned")
            .clone()
    }

    /// Every create body this recorder was sent.
    fn creates(&self) -> Vec<String> {
        self.creates
            .lock()
            .expect("the creates mutex is poisoned")
            .clone()
    }

    /// The one create body this recorder was sent.
    fn recorded(&self) -> String {
        let creates = self.creates();
        let [body] = &creates[..] else {
            panic!("expected exactly one forwarded create, got {creates:?}");
        };
        body.clone()
    }

    fn stop(self) {
        self.serving.abort();
    }
}

// ---------------------------------------------------------------------------
// Splicing a client's sessions (spec 7.1, 6.5, 6.10)
// ---------------------------------------------------------------------------

/// A real turn on a real host, watched through a gateway.
///
/// The whole composed path: the client's attach travels upstream, the host's
/// block and the turn it drives come back, and every session-scoped frame names
/// the id the client asked for rather than the one the host knows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_spliced_turn_reaches_a_client_with_its_ids_namespaced() {
    let mut host = Upstream::start().await;
    let session = host.create().await;
    let fixture = Fixture::new(&[&host]).await;
    let id = host.namespaced(&session);
    fixture.row(&id).await;

    let mut events = fixture.attach(&[attach(&id)]).await;

    let block = frames_until(&mut events, "the attach block", is_caught_up).await;
    assert!(
        block
            .iter()
            .any(|frame| matches!(frame, Frame::State { .. })),
        "an attach block opens with the session's state (spec 6.5): {block:?}",
    );
    assert!(
        named_sessions(&block).iter().all(|named| *named == id),
        "a spliced frame names the session the client attached, not the host's own \
         id ({session}): {:?}",
        named_sessions(&block),
    );

    fixture
        .client
        .command(&id, &prompt("go"))
        .await
        .expect("the prompt is accepted");
    let turn = frames_until(&mut events, "the assistant's answer", |frame| {
        !assistant_text(std::slice::from_ref(frame)).is_empty()
    })
    .await;

    assert_eq!(
        assistant_text(&turn),
        vec!["done".to_string()],
        "the turn the client drove came back on the stream it was watching",
    );
    assert!(
        named_sessions(&turn).iter().all(|named| *named == id),
        "and its frames are namespaced too: {:?}",
        named_sessions(&turn),
    );
    assert!(
        !durable_seqs(&turn).is_empty(),
        "the durable envelope travels with them, which is what advances a cursor: {turn:?}",
    );

    fixture.shutdown().await;
    host.stop().await;
}

/// One client stream, two hosts: each session's frames arrive under its own
/// host's namespace, and neither host's stream carries the other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hosts_ride_one_client_stream() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let here = left.create().await;
    let there = right.create().await;
    let fixture = Fixture::new(&[&left, &right]).await;
    let (here, there) = (left.namespaced(&here), right.namespaced(&there));
    fixture.row(&here).await;
    fixture.row(&there).await;

    let mut events = fixture.attach(&[attach(&here), attach(&there)]).await;

    let mut blocks = 0;
    let opened = frames_until(&mut events, "both attach blocks", |frame| {
        if is_caught_up(frame) {
            blocks += 1;
        }
        blocks == 2
    })
    .await;
    let mut named: Vec<&str> = named_sessions(&opened);
    named.sort_unstable();
    named.dedup();
    assert_eq!(
        named,
        {
            let mut both = vec![here.as_str(), there.as_str()];
            both.sort_unstable();
            both
        },
        "one block per session, each under its own host: {opened:?}",
    );

    for (id, text) in [(&here, "done"), (&there, "done")] {
        fixture
            .client
            .command(id, &prompt("go"))
            .await
            .expect("the prompt is accepted");
        let turn = frames_until(&mut events, "the answer", |frame| {
            !assistant_text(std::slice::from_ref(frame)).is_empty()
        })
        .await;
        assert_eq!(assistant_text(&turn), vec![text.to_string()]);
        assert!(
            turn.iter()
                .filter_map(Frame::session)
                .any(|named| named == id),
            "the turn arrived under {id}: {turn:?}",
        );
    }

    fixture.shutdown().await;
    left.stop().await;
    right.stop().await;
}

/// What travels upstream is the host's own ids with the client's own cursors,
/// one stream per host (spec 7.1).
///
/// A real host would answer an attach either way, so only a host that keeps what
/// it was asked can show the difference: a gateway that forwarded namespaced ids,
/// or dropped the cursors, or opened a stream per session, would be invisible
/// otherwise.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upstream_attach_carries_host_ids_and_the_clients_cursors() {
    let fake = FakeHost::start("fake", Script::Frames(block("s-1", "epoch-1", 3))).await;
    let fixture = Fixture::over(TempDir::new().expect("tempdir"), vec![fake.address.clone()]).await;
    fixture.until_connected("fake").await;

    let mut events = fixture
        .attach(&[
            attach_at(
                "fake:s-1",
                Cursor {
                    epoch: "epoch-1".to_string(),
                    seq: 3,
                },
            ),
            attach("fake:s-2"),
        ])
        .await;
    frames_until(&mut events, "the host's block", is_caught_up).await;

    let attaches = fake.attaches();
    assert_eq!(
        attaches
            .iter()
            .filter(|attached| !attached.is_empty())
            .collect::<Vec<_>>(),
        vec![&vec!["s-1@epoch-1:3".to_string(), "s-2".to_string()]],
        "one upstream for the host, carrying its own ids and the client's own \
         cursors: {attaches:?}",
    );
    assert!(
        attaches.iter().any(|attached| attached.is_empty()),
        "and the control connection attaches nothing at all (spec 7.1): {attaches:?}",
    );

    fixture.shutdown().await;
    fake.stop();
}

/// An id this gateway cannot resolve is refused before any host is asked about
/// it, which is the same 404 a proxied request to it answers (spec 6.2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attaching_an_id_this_gateway_cannot_name_reaches_no_host() {
    let fake = FakeHost::start("fake", Script::Frames(block("s-1", "epoch-1", 0))).await;
    let fixture = Fixture::over(TempDir::new().expect("tempdir"), vec![fake.address.clone()]).await;
    fixture.until_connected("fake").await;

    for id in [
        // A bare host-local id: what a host answers to, and meaningless here.
        "s-1",
        // A host this gateway does not have.
        "0123456789abcdef:whatever",
        ":whatever",
        "fake:",
        // A session half a URL would swallow.
        "fake:..",
    ] {
        let mut events = fixture
            .client
            .events(&[attach(id)])
            .await
            .unwrap_or_else(|err| panic!("{id:?} did not open a stream: {err}"));

        // The host is read before the frames are, so an attach that got through
        // fails on having reached a host rather than on the shape of what came
        // back.
        assert_eq!(
            fake.spliced_attaches(),
            Vec::<Vec<String>>::new(),
            "attaching {id:?} reached a host this gateway cannot address it on",
        );
        let (refused, code, _) = refused_session(&mut events).await;
        assert_eq!(refused, id, "the refusal names the id the client sent");
        assert_eq!(code, "unknown_session", "{id:?}");
    }

    fixture.shutdown().await;
    fake.stop();
}

/// A client's stream never fails wholesale over one bad session (spec 6.5,
/// 7.1): the sessions this gateway can resolve are served, on every host they
/// live on, and each id it cannot resolve is refused on that same stream.
///
/// Two hosts, because that is what the rule is about: failing the stream over
/// one dead id costs a client its healthy sessions on every *other* host, and
/// a single upstream cannot show that.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_refuses_the_ids_it_cannot_resolve_and_serves_both_hosts() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let here = left.create().await;
    let there = right.create().await;
    let fixture = Fixture::new(&[&left, &right]).await;
    let (here, there) = (left.namespaced(&here), right.namespaced(&there));
    fixture.row(&here).await;
    fixture.row(&there).await;
    // An id under this gateway's own namespace that its owning host does not
    // hold, so the refusal below is this gateway's and not that host's.
    let gone = left.namespaced("20260101-000000-000");

    let mut events = fixture
        .attach(&[
            attach(&here),
            // No host of this gateway answers to that namespace.
            attach("0123456789abcdef:whatever"),
            attach(&there),
            attach(&gone),
        ])
        .await;

    let mut blocks = 0;
    let mut refusals = 0;
    let opened = frames_until(&mut events, "both blocks and both refusals", |frame| {
        match frame {
            Frame::CaughtUp { .. } => blocks += 1,
            Frame::Error { .. } => refusals += 1,
            _ => {}
        }
        blocks == 2 && refusals == 2
    })
    .await;
    let mut served: Vec<&str> = opened
        .iter()
        .filter(|frame| is_caught_up(frame))
        .filter_map(Frame::session)
        .collect();
    served.sort_unstable();
    assert_eq!(
        served,
        {
            let mut both = vec![here.as_str(), there.as_str()];
            both.sort_unstable();
            both
        },
        "both hosts' sessions were served on the stream a bad id shared: \
         {opened:?}",
    );
    let refused: Vec<(&str, &str)> = opened
        .iter()
        .filter_map(|frame| match frame {
            Frame::Error { session, code, .. } => Some((session.as_str(), code.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        refused,
        vec![
            ("0123456789abcdef:whatever", "unknown_session"),
            (gone.as_str(), "unknown_session"),
        ],
        "each unresolvable id is refused by the name the client gave it, in \
         this gateway's own namespaced vocabulary, whether this gateway or the \
         owning host is the one that could not resolve it: {opened:?}",
    );

    // And the stream is a working stream afterwards, not one limping to its
    // end: a turn on either host still arrives.
    for (id, text) in [(&here, "done"), (&there, "done")] {
        fixture
            .client
            .command(id, &prompt("go"))
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "the post-attach prompt for {id} failed before waiting for its assistant \
                     answer. Request timeouts here are real-time and load-sensitive: {err}"
                )
            });
        let turn = frames_until(&mut events, "the answer", |frame| {
            !assistant_text(std::slice::from_ref(frame)).is_empty()
        })
        .await;
        assert_eq!(assistant_text(&turn), vec![text.to_string()]);
    }

    fixture.shutdown().await;
    left.stop().await;
    right.stop().await;
}

/// The refusals an attach owes are answers to that attach, not live fan-out: a
/// client naming more ids this gateway cannot resolve than its queue can hold is
/// served every one of them, and the sessions it holds on real hosts with them
/// (spec 6.5, 7.1).
///
/// A client whose refusals travelled its own bounded queue would be evicted by
/// its own attach, and the re-attach it made to recover would be evicted the
/// same way: a sidebar restored with stale ids could never attach again at all.
///
/// Two hosts, because what a stream failing here costs is every session on every
/// other host. The bound is deliberately smaller than the number of dead ids and
/// checked against it, because a queue that could hold them all would measure
/// nothing. No turn is driven afterwards: a bound of two makes live fan-out a
/// race of its own, and what this is about is the attach.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn more_dead_ids_than_the_bound_do_not_evict_the_client_that_named_them() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let here = left.create().await;
    let there = right.create().await;
    let bound = NonZeroUsize::new(2).expect("non-zero");
    let fixture = Fixture::tuned(
        TempDir::new().expect("tempdir"),
        [&left, &right]
            .iter()
            .map(|host| HostAddress::parse(&host.address()).expect("a host address"))
            .collect(),
        Tuning {
            outbound_queue: bound,
            ..tuning()
        },
    )
    .await;
    let (here, there) = (left.namespaced(&here), right.namespaced(&there));
    fixture.row(&here).await;
    fixture.row(&there).await;
    // Ids under namespaces no host here answers to, so each is this gateway's own
    // refusal rather than a host's.
    let dead: Vec<String> = (0..8).map(|n| format!("0123456789abcde{n}:gone")).collect();
    assert!(
        dead.len() > bound.get(),
        "{} dead ids against a bound of {}: a queue that holds them all is not \
         the queue this test is about",
        dead.len(),
        bound.get(),
    );

    let attaching: Vec<AttachRequest> = dead
        .iter()
        .map(|id| attach(id))
        .chain([attach(&here), attach(&there)])
        .collect();
    let mut events = fixture.attach(&attaching).await;

    // Every refusal and both blocks. A stream evicted by its own attach ends
    // here, which is the harm this is about.
    let mut blocks = 0;
    let mut refusals = 0;
    let opened = frames_until(&mut events, "every refusal and both blocks", |frame| {
        match frame {
            Frame::CaughtUp { .. } => blocks += 1,
            Frame::Error { .. } => refusals += 1,
            _ => {}
        }
        blocks == 2 && refusals == dead.len()
    })
    .await;
    assert_eq!(
        refused_sessions(opened.iter()),
        dead.iter().map(String::as_str).collect::<Vec<_>>(),
        "each id the client named, refused by the name it gave it: {opened:?}",
    );
    let mut served: Vec<&str> = opened
        .iter()
        .filter(|frame| is_caught_up(frame))
        .filter_map(Frame::session)
        .collect();
    served.sort_unstable();
    assert_eq!(
        served,
        {
            let mut both = vec![here.as_str(), there.as_str()];
            both.sort_unstable();
            both
        },
        "and both hosts' sessions served on the stream those refusals shared: \
         {opened:?}",
    );

    fixture.shutdown().await;
    left.stop().await;
    right.stop().await;
}

/// The owning host's refusal of an attach travels back, code and all: the client
/// asked the question and the host answered it (spec 6.10).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hosts_own_attach_refusal_travels_back() {
    let fake = FakeHost::start("fake", Script::Refuse).await;
    let fixture = Fixture::over(TempDir::new().expect("tempdir"), vec![fake.address.clone()]).await;
    fixture.until_connected("fake").await;

    let Err(err) = fixture.client.events(&[attach("fake:s-1")]).await else {
        panic!("the host refuses this attach");
    };

    assert_eq!(err.status(), Some(StatusCode::CONFLICT), "got {err:?}");
    assert_eq!(
        err.code(),
        Some("locked"),
        "the host's own code, which this gateway has no vocabulary of its own for",
    );
    assert!(
        err.to_string().contains("another writer"),
        "and the host's own words: {err}",
    );

    fixture.shutdown().await;
    fake.stop();
}

/// A refusal an owning host wrote reaches the client whole (spec 6.6).
///
/// Three things about the same body: an envelope carrying only a `message` is a
/// complete error, so the message is the host's sentence and not the JSON it
/// arrived in; the session it names is named in the one vocabulary a client of
/// this gateway has; and a field this build has no name for is still there for a
/// client that does. The proxy carries a host's error bodies whole, and this is
/// the same rule on the path that does not go through it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hosts_own_attach_refusal_travels_back_whole() {
    let fake = FakeHost::start(
        "fake",
        Script::RefuseRaw(
            r#"{"message":"the session is held by another writer","session":"s-1","holder":"pid 42"}"#,
        ),
    )
    .await;
    let fixture = Fixture::over(TempDir::new().expect("tempdir"), vec![fake.address.clone()]).await;
    fixture.until_connected("fake").await;

    let response = fixture
        .http
        .get(format!(
            "{}/v1/events?session=fake:s-1",
            fixture.server.url()
        ))
        .send()
        .await
        .expect("the stream request");

    let status = response.status();
    let body: serde_json::Value = response.json().await.expect("a JSON body");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["message"], "the session is held by another writer",
        "the host's own sentence rather than the body it arrived in: {body}",
    );
    assert_eq!(
        body["session"], "fake:s-1",
        "named in the only vocabulary a client of this gateway has: {body}",
    );
    assert_eq!(
        body["holder"], "pid 42",
        "and a field this gateway has no name for is still there for a client \
         that has: {body}",
    );

    fixture.shutdown().await;
    fake.stop();
}

/// A frame kind this build does not know is forwarded with its session id
/// rewritten and nothing else touched (spec 6.10's forward-don't-filter).
///
/// The two frames that do not travel are in the same script: a host's own `list`
/// would put ids no client of this gateway can address on the stream, and a
/// heartbeat belongs to the connection it was written on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_frame_kind_is_forwarded_with_its_session_rewritten() {
    let mut script = block("s-1", "epoch-1", 0);
    script.push(serde_json::to_string(&Frame::Heartbeat).expect("a heartbeat"));
    script.push(
        serde_json::to_string(&Frame::List {
            sessions: vec![fake_row("s-1")],
            hosts: Vec::new(),
        })
        .expect("a list frame"),
    );
    script.push(
        r#"{"kind":"something_newer","session":"s-1","payload":{"n":18446744073709551616}}"#
            .to_string(),
    );
    // No `session` at all, which makes it host-scoped: forwarded as it arrived
    // (spec 6.10).
    script.push(r#"{"kind":"something_global","note":"host wide"}"#.to_string());
    // A host's own `reset`, which a head switch produces (spec 6.3): the gateway
    // has its own reasons to emit one, and that is no reason to swallow this.
    script.push(
        serde_json::to_string(&Frame::Reset {
            session: "s-1".to_string(),
        })
        .expect("a reset frame"),
    );
    script.push(warning_frame("s-1", "epoch-1", "the frame after it"));
    let fake = FakeHost::start("fake", Script::Frames(script)).await;
    let fixture = Fixture::over(TempDir::new().expect("tempdir"), vec![fake.address.clone()]).await;
    fixture.until_connected("fake").await;

    let mut events = fixture.attach(&[attach("fake:s-1")]).await;
    let seen = decoded_until(&mut events, "the frame behind the unknown one", |frame| {
        matches!(frame, DecodedFrame::Known(known)
            if matches!(known.value(), Frame::Event { .. }))
    })
    .await;

    let unknown: Vec<&DecodedFrame> = seen
        .iter()
        .filter(|frame| matches!(frame, DecodedFrame::Unknown { .. }))
        .collect();
    let [forwarded, host_scoped] = unknown[..] else {
        panic!("an unknown kind was filtered out rather than forwarded: {seen:?}");
    };
    let DecodedFrame::Unknown { kind, raw } = forwarded else {
        unreachable!("filtered on the variant");
    };
    assert_eq!(kind, "something_newer");
    assert_eq!(
        forwarded
            .session()
            .expect("a readable session id")
            .as_deref(),
        Some("fake:s-1"),
        "a kind this gateway cannot read still gets its id namespaced: {}",
        raw.get(),
    );
    assert!(
        raw.get().contains("18446744073709551616"),
        "and its payload travels verbatim, number literals included: {}",
        raw.get(),
    );
    let DecodedFrame::Unknown {
        kind: host_kind,
        raw: host_raw,
    } = host_scoped
    else {
        unreachable!("filtered on the variant");
    };
    assert_eq!(host_kind, "something_global");
    assert!(
        host_raw.get().contains("host wide"),
        "an unknown kind that names no session is host-scoped, and travels as it \
         arrived: {}",
        host_raw.get(),
    );

    let known: Vec<&Frame> = seen
        .iter()
        .filter_map(|frame| match frame {
            DecodedFrame::Known(known) => Some(known.value()),
            DecodedFrame::Unknown { .. } => None,
        })
        .collect();
    assert!(
        !known.iter().any(|frame| matches!(frame, Frame::Heartbeat)),
        "a host's heartbeat is not a client's: {known:?}",
    );
    assert_eq!(
        known
            .iter()
            .filter_map(|frame| match frame {
                Frame::Reset { session } => Some(session.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["fake:s-1"],
        "a host's own reset travels too, namespaced: {known:?}",
    );
    for frame in &known {
        if let Frame::List { sessions, .. } = frame {
            for row in sessions {
                SessionAddress::parse(&row.id).unwrap_or_else(|err| {
                    panic!(
                        "a host's own list reached the client, so {:?} is not addressable \
                         here: {err}",
                        row.id
                    )
                });
            }
        }
    }

    fixture.shutdown().await;
    fake.stop();
}

// ---------------------------------------------------------------------------
// A host that flaps (spec 7.1's `reset`)
// ---------------------------------------------------------------------------

/// A host that goes away resets exactly its own sessions, and the sessions of
/// the host that stayed keep working on the very same stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_host_resets_its_own_sessions_and_leaves_the_others_alone() {
    let mut left = Upstream::start().await;
    let mut right = Upstream::start().await;
    let doomed = left.create().await;
    let alive = right.create().await;
    let fixture = Fixture::new(&[&left, &right]).await;
    let (doomed, alive) = (left.namespaced(&doomed), right.namespaced(&alive));
    fixture.row(&doomed).await;
    fixture.row(&alive).await;
    let mut events = fixture.attach(&[attach(&doomed), attach(&alive)]).await;
    let mut blocks = 0;
    frames_until(&mut events, "both attach blocks", |frame| {
        if is_caught_up(frame) {
            blocks += 1;
        }
        blocks == 2
    })
    .await;

    left.stop().await;

    let lost = frames_until(&mut events, "a reset for the lost host", |frame| {
        matches!(frame, Frame::Reset { .. })
    })
    .await;
    assert_eq!(
        resets(&lost),
        vec![doomed.clone()],
        "the sessions of the host that went away, and no others: {lost:?}",
    );

    // The other host's session is untouched: it is watched over a stream of its
    // own, which the flap next door never touched.
    fixture
        .client
        .command(&alive, &prompt("go"))
        .await
        .expect("the prompt is accepted");
    let turn = frames_until(&mut events, "the healthy host's answer", |frame| {
        !assistant_text(std::slice::from_ref(frame)).is_empty()
    })
    .await;
    assert_eq!(assistant_text(&turn), vec!["done".to_string()]);
    assert!(
        !resets(&turn).contains(&alive),
        "the healthy host's session was reset over another host's flap: {turn:?}",
    );

    fixture.shutdown().await;
    right.stop().await;
}

/// A gateway does not reopen an upstream it lost. It says `reset` and waits for
/// the client to attach again (spec 7.1).
///
/// Resuming one itself would need a *current* cursor, and the client's cursor
/// advances as it applies what this gateway forwarded, so the gateway would have
/// to keep per-session cursor state that spec 7.1 forbids it. A host that hangs a
/// spliced stream up while its control connection stays open is that case with
/// nothing else moving: exactly one upstream was ever opened, and exactly one
/// `reset` came back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gateway_does_not_reopen_an_upstream_it_lost() {
    let fake = FakeHost::start("fake", Script::Ends(block("s-1", "epoch-1", 0))).await;
    let fixture = Fixture::over(TempDir::new().expect("tempdir"), vec![fake.address.clone()]).await;
    fixture.until_connected("fake").await;

    let mut events = fixture.attach(&[attach("fake:s-1")]).await;
    let lost = frames_until(&mut events, "the reset for the ended stream", |frame| {
        matches!(frame, Frame::Reset { .. })
    })
    .await;
    assert_eq!(resets(&lost), vec!["fake:s-1".to_string()]);

    let quiet = frames_within(&mut events, QUIET).await;

    assert_eq!(
        fake.spliced_attaches().len(),
        1,
        "the gateway attached again by itself, which it has no cursor to do \
         honestly: {:?}",
        fake.spliced_attaches(),
    );
    assert!(
        resets(&quiet).is_empty(),
        "and it does not repeat itself while it waits: {quiet:?}",
    );

    fixture.shutdown().await;
    fake.stop();
}

/// A re-attach while the host is still down does not fail the stream, does not
/// spin on `reset`, and is told when the host comes back (spec 7.1).
///
/// Unreachable is **pending**, and the contrast with an id this gateway cannot
/// resolve is what the same stream carries here: the unresolvable one is
/// refused with an `error` frame, the unreachable one gets none and is held.
/// Collapsing the two would either drop a client's attachment over a flap that
/// is about to heal, or leave it waiting for frames that can never come.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reattach_while_a_host_is_down_waits_for_it_to_return() {
    let mut down = Upstream::start().await;
    let mut up = Upstream::start().await;
    let waiting = down.create().await;
    let watched = up.create().await;
    let fixture = Fixture::new(&[&down, &up]).await;
    let (waiting, watched) = (down.namespaced(&waiting), up.namespaced(&watched));
    fixture.row(&waiting).await;
    fixture.row(&watched).await;
    down.stop().await;
    fixture
        .until("the downed host's row to be marked", |list| {
            list.sessions
                .iter()
                .find(|row| row.id == waiting && row.unreachable)
                .map(|_| ())
        })
        .await;
    // An id nothing here resolves, on the same stream, so that "no refusal for
    // the unreachable one" measures the distinction rather than a gateway that
    // refuses nothing at all.
    let gone = "0123456789abcdef:whatever";

    let mut events = fixture
        .attach(&[attach(&waiting), attach(&watched), attach(gone)])
        .await;

    // The healthy host's session is served, which is what failing the whole
    // stream over its neighbour would have cost.
    let opened = frames_until(&mut events, "the healthy session's block", is_caught_up).await;
    assert!(
        named_sessions(&opened).contains(&watched.as_str()),
        "the session on the host that is there was served: {opened:?}",
    );
    let quiet = frames_within(&mut events, QUIET).await;
    assert!(
        resets(&quiet).is_empty(),
        "a session whose host is known to be down waits rather than being reset \
         over and over: {quiet:?}",
    );
    assert_eq!(
        refused_sessions(opened.iter().chain(quiet.iter())),
        vec![gone],
        "an unreachable host's session is pending and is owed no refusal, while \
         an id this gateway cannot resolve is owed one: {opened:?} {quiet:?}",
    );

    down.restart().await;

    let returned = frames_until(&mut events, "a reset for the returned host", |frame| {
        matches!(frame, Frame::Reset { .. })
    })
    .await;
    assert_eq!(
        resets(&returned),
        vec![waiting.clone()],
        "the host came back, so its sessions are asked to attach again: {returned:?}",
    );
    // And the window a second `reset` would have arrived in. Which reset comes
    // first is the order of a map over host ids, so reading until the first one
    // says nothing about the session that never lost continuity: being asked to
    // re-attach that one costs the client a backfill it did not need.
    let others = frames_within(&mut events, QUIET).await;
    assert!(
        !resets(&others).contains(&watched),
        "one host's return reset a session on the host that was there all along: \
         {others:?}",
    );

    fixture.shutdown().await;
    down.stop().await;
    up.stop().await;
}

/// A flap the host itself survived resumes **incrementally**: the client's
/// cursor still means what it meant, so the host serves the suffix after it
/// (spec 7.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reattach_after_a_reset_resumes_incrementally_when_the_epoch_survived() {
    let mut host = Upstream::start().await;
    let session = host.create().await;
    let bridge = Bridge::to(&host).await;
    let fixture = Fixture::over(
        TempDir::new().expect("tempdir"),
        vec![bridge.address.clone()],
    )
    .await;
    let id = host.namespaced(&session);
    fixture.row(&id).await;
    // A turn before the attach, so the block carries durable frames a resume
    // could wrongly serve again.
    let before = host.durable_seq(&session).await;
    fixture
        .client
        .command(&id, &prompt("first"))
        .await
        .expect("the prompt is accepted");
    settled(&host, &session, before + 1).await;

    let mut events = fixture.attach(&[attach(&id)]).await;
    let block = frames_until(&mut events, "the first attach block", is_caught_up).await;
    let epoch = epoch_of(&block);
    let cursor = Cursor {
        epoch: epoch.clone(),
        seq: caught_up_at(&block),
    };
    assert_eq!(
        assistant_text(&block),
        vec!["done".to_string()],
        "the first turn is in the block, so re-serving it would show: {block:?}",
    );

    bridge.cut();
    let lost = frames_until(&mut events, "the reset for the broken link", |frame| {
        matches!(frame, Frame::Reset { .. })
    })
    .await;
    assert_eq!(resets(&lost), vec![id.clone()]);

    // The host never went anywhere: its session runs on while this gateway
    // cannot see it, which is what makes the resume incremental rather than
    // empty.
    host.prompt(&session, "second").await;
    settled(&host, &session, cursor.seq + 1).await;
    let reached = host.durable_seq(&session).await;
    bridge.heal();
    fixture.until_connected(&host.host_id()).await;

    drop(events);
    let mut events = fixture.attach(&[attach_at(&id, cursor.clone())]).await;
    let resumed = frames_until(
        &mut events,
        "the resumed attach body to reach caught_up after the healed host reconnected and the \
         response head opened",
        is_caught_up,
    )
    .await;

    assert_eq!(
        epoch_of(&resumed),
        epoch,
        "the host's epoch survived the flap, so the client's cursor still means \
         something: {resumed:?}",
    );
    assert!(
        durable_seqs(&resumed).iter().all(|seq| *seq > cursor.seq),
        "the suffix after the cursor and nothing below it, which is what forwarding \
         the client's own cursor buys: {:?} against a cursor at {}",
        durable_seqs(&resumed),
        cursor.seq,
    );
    assert_eq!(
        assistant_text(&resumed),
        vec!["done".to_string()],
        "the turn it missed, once, rather than both turns again: {resumed:?}",
    );
    assert_eq!(caught_up_at(&resumed), reached);

    // A second flap, now that this stream was opened with a cursor: what the
    // `reset` names is the session, not the session with the cursor stuck to it.
    bridge.cut();
    let again = frames_until(&mut events, "the reset for the second flap", |frame| {
        matches!(frame, Frame::Reset { .. })
    })
    .await;
    assert_eq!(
        resets(&again),
        vec![id.clone()],
        "a client matches a reset against the id it attached: {again:?}",
    );

    fixture.shutdown().await;
    bridge.stop();
    host.stop().await;
}

/// A host that restarted mints fresh epochs, so the same re-attach resumes
/// **fully**: the cursor describes a history the session no longer has
/// (spec 6.5, 7.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reattach_after_a_restart_resumes_fully_when_the_epoch_changed() {
    let mut host = Upstream::start().await;
    let session = host.create().await;
    let fixture = Fixture::new(&[&host]).await;
    let id = host.namespaced(&session);
    fixture.row(&id).await;
    let before = host.durable_seq(&session).await;
    fixture
        .client
        .command(&id, &prompt("first"))
        .await
        .expect("the prompt is accepted");
    settled(&host, &session, before + 1).await;

    let mut events = fixture.attach(&[attach(&id)]).await;
    let block = frames_until(&mut events, "the first attach block", is_caught_up).await;
    let epoch = epoch_of(&block);
    let cursor = Cursor {
        epoch: epoch.clone(),
        seq: caught_up_at(&block),
    };
    assert!(
        durable_seqs(&block).iter().any(|seq| *seq <= cursor.seq),
        "the log has durable entries at or below the cursor: {block:?}",
    );

    host.stop().await;
    frames_until(&mut events, "the reset for the lost host", |frame| {
        matches!(frame, Frame::Reset { .. })
    })
    .await;
    host.restart().await;
    fixture.until_connected(&host.host_id()).await;

    drop(events);
    let mut events = fixture.attach(&[attach_at(&id, cursor.clone())]).await;
    let resumed = frames_until(&mut events, "the resumed attach block", is_caught_up).await;

    assert_ne!(
        epoch_of(&resumed),
        epoch,
        "a restart materializes the session afresh, so its epoch is new: {resumed:?}",
    );
    assert!(
        durable_seqs(&resumed).iter().any(|seq| *seq <= cursor.seq),
        "a cursor from an epoch that is gone earns the whole log back: {:?} against a \
         cursor at {}",
        durable_seqs(&resumed),
        cursor.seq,
    );
    assert_eq!(
        assistant_text(&resumed),
        vec!["done".to_string()],
        "the turn from before the restart is served again: {resumed:?}",
    );

    fixture.shutdown().await;
    host.stop().await;
}

// ---------------------------------------------------------------------------
// Withdrawing an enrollment (spec 7.1's active teardown)
// ---------------------------------------------------------------------------

/// Removing an enrollment is active teardown, not bookkeeping (spec 7.1): the
/// host's rows leave the merged list, its upstream connections close, and its
/// splices end with the `reset` a withdrawal owes them. Leaving them running
/// would serve a directory that contradicts the enrollment set.
///
/// Two hosts on one client stream, because a client's attach set spans hosts,
/// so ending one host's splice must not cost that client the sessions it holds
/// on the others, and the `reset` must name the withdrawn host's sessions and
/// no others: a client that re-attached everything it holds would take a
/// perfectly healthy session through a needless full backfill.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withdrawal_ends_that_hosts_splices_and_leaves_the_others_alone() {
    let cue = Arc::new(tokio::sync::Notify::new());
    let leaving = FakeHost::start("leaving", Script::Frames(block("s-1", "epoch-1", 0))).await;
    let staying = FakeHost::start(
        "staying",
        Script::Cued {
            first: block("s-9", "epoch-9", 0),
            cue: Arc::clone(&cue),
            then: vec![warning_frame("s-9", "epoch-9", "still watching this one")],
        },
    )
    .await;
    // Enrolled over the wire: a configured host is the file's to remove, and
    // refusing to withdraw one is its own test.
    let fixture = Fixture::new(&[]).await;
    for host in [&leaving, &staying] {
        assert_eq!(
            fixture.enroll(&host.address.to_string()).await.status(),
            StatusCode::OK,
        );
    }
    // An enrollment answers before its link has dialed, and a host this gateway
    // holds no link to contributes no upstream at all (spec 7.1).
    fixture.until_connected("leaving").await;
    fixture.until_connected("staying").await;
    let mut events = fixture
        .attach(&[attach("leaving:s-1"), attach("staying:s-9")])
        .await;
    let opened = carried_until(&mut events, "both attach blocks", |carried| {
        carried.caught_up.len() == 2
    })
    .await;
    assert_eq!(
        leaving.spliced_attaches().len(),
        1,
        "the upstream this withdrawal has to end was never opened, so nothing \
         here is torn down: {opened:?}",
    );
    assert_eq!(
        (leaving.released(), staying.released()),
        (0, 0),
        "and both are still open going in",
    );

    assert_eq!(
        fixture.withdraw("leaving").await.status(),
        StatusCode::NO_CONTENT,
    );

    // The `reset` the withdrawal owes, naming the withdrawn host's session and
    // that one alone (spec 7.1): continuity for it is over, and re-attaching is
    // how the client is told what became of it (spec 6.5).
    let torn_down = carried_until(&mut events, "the reset a withdrawal owes", |carried| {
        !carried.resets.is_empty()
    })
    .await;
    assert_eq!(
        torn_down.resets,
        vec!["leaving:s-1".to_string()],
        "a withdrawal reset something other than the sessions of the host it \
         withdrew: {torn_down:?}",
    );
    assert!(
        !torn_down.ended,
        "the client's whole stream ended over one host's withdrawal: {torn_down:?}",
    );

    bounded("the withdrawn host's upstream to close", async {
        while leaving.released() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert_eq!(
        staying.released(),
        0,
        "the other host's stream went down with it, which is the collateral a \
         client's attach set spanning hosts cannot afford",
    );

    // The client is still there and still served: a live frame from the host
    // that stayed, which also bounds the window a second `reset` would have
    // arrived in.
    cue.notify_one();
    let carried = carried_until(
        &mut events,
        "a frame from the host that stayed",
        |carried| carried.events("staying:s-9") > 0,
    )
    .await;
    assert!(
        !carried.ended,
        "the client's whole stream ended over one host's withdrawal: {carried:?}",
    );
    assert!(
        carried.resets.is_empty(),
        "the withdrawal reset a session on the host it left alone: {carried:?}",
    );

    // And the directory says the same thing the teardown did.
    fixture
        .until("the withdrawn host's rows to go", |list| {
            (!list
                .sessions
                .iter()
                .any(|row| row.host.as_deref() == Some("leaving"))
                && list
                    .sessions
                    .iter()
                    .any(|row| row.host.as_deref() == Some("staying")))
            .then_some(())
        })
        .await;
    assert_eq!(
        fixture
            .client
            .sessions()
            .await
            .expect("the merged directory")
            .hosts,
        vec![DirectoryHost {
            id: Some("staying".to_string()),
            address: None,
            name: None,
            unreachable: false,
        }],
        "a host that is not enrolled is not a group either, and the one that is \
         still names itself",
    );
    let err = fixture
        .client
        .command("leaving:s-1", &prompt("go"))
        .await
        .expect_err("a session on a withdrawn host names nothing here");
    assert_eq!(err.status(), Some(StatusCode::NOT_FOUND), "got {err:?}");
    assert_eq!(err.code(), Some("unknown_session"));

    fixture.shutdown().await;
    leaving.stop();
    staying.stop();
}

/// The whole sequence a withdrawal sets off (spec 7.1): `reset`, re-attach,
/// per-session refusal, the attachment dropped, healthy hosts untouched.
///
/// The refusal being per session is what makes the `reset` safe to send at all.
/// One that failed the client's stream would cost it every session it holds on
/// every other host, so the re-attach here names a session on the host that
/// stayed as well: that one has to be served on the same stream that refuses the
/// withdrawn one, and nothing more may arrive for the id that was refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_re_attach_after_a_withdrawal_is_refused_for_that_session_alone() {
    let leaving = FakeHost::start("leaving", Script::Frames(block("s-1", "epoch-1", 0))).await;
    let staying = FakeHost::start("staying", Script::Frames(block("s-9", "epoch-9", 0))).await;
    // Enrolled over the wire: a configured host is the file's to remove.
    let fixture = Fixture::new(&[]).await;
    for host in [&leaving, &staying] {
        assert_eq!(
            fixture.enroll(&host.address.to_string()).await.status(),
            StatusCode::OK,
        );
    }
    fixture.until_connected("leaving").await;
    fixture.until_connected("staying").await;
    let mut events = fixture
        .attach(&[attach("leaving:s-1"), attach("staying:s-9")])
        .await;
    let opened = carried_until(&mut events, "both attach blocks", |carried| {
        carried.caught_up.len() == 2
    })
    .await;
    assert_eq!(
        leaving.spliced_attaches().len(),
        1,
        "the upstream whose end owes this reset was never opened: {opened:?}",
    );

    assert_eq!(
        fixture.withdraw("leaving").await.status(),
        StatusCode::NO_CONTENT,
    );
    let torn_down = carried_until(&mut events, "the reset a withdrawal owes", |carried| {
        !carried.resets.is_empty()
    })
    .await;
    assert_eq!(torn_down.resets, vec!["leaving:s-1".to_string()]);

    // What a client does with a `reset`: reopen the stream naming the session it
    // was sent for. It names everything it holds, because changing the attach
    // set means reopening the stream (spec 6.5), so its healthy session travels
    // with the refused one.
    let mut resumed = fixture
        .client
        .events(&[attach("leaving:s-1"), attach("staying:s-9")])
        .await
        .expect(
            "the re-attach failed the client's whole stream over one withdrawn \
             host, which costs it the sessions it holds on every other one",
        );
    let served = frames_until(
        &mut resumed,
        "the block for the host that stayed",
        |frame| matches!(frame, Frame::CaughtUp { .. }),
    )
    .await;
    assert_eq!(
        served
            .iter()
            .filter_map(|frame| match frame {
                Frame::CaughtUp { session, .. } => Some(session.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["staying:s-9"],
        "the host that was never withdrawn was not served: {served:?}",
    );
    let Some(Frame::Error {
        session,
        epoch,
        code,
        message,
        ..
    }) = served
        .iter()
        .find(|frame| matches!(frame, Frame::Error { .. }))
    else {
        panic!("the re-attach of a withdrawn host's session is refused: {served:?}");
    };
    assert_eq!(session, "leaving:s-1", "named as the client named it");
    assert_eq!(code, "unknown_session");
    assert_eq!(
        *epoch, None,
        "nothing resolved, so no epoch was ever minted for it here",
    );
    assert!(
        message.contains("no host leaving is enrolled here"),
        "the refusal says why: {message}",
    );

    // And that is the end of it: an attachment this gateway refused carries
    // nothing afterwards, so the client that dropped it misses nothing.
    let after = frames_within(&mut resumed, QUIET).await;
    assert_eq!(
        named_sessions(&after)
            .into_iter()
            .filter(|session| session.starts_with("leaving:"))
            .collect::<Vec<_>>(),
        Vec::<&str>::new(),
        "the refused attachment went on carrying frames: {after:?}",
    );

    fixture.shutdown().await;
    leaving.stop();
    staying.stop();
}

/// The last step of a withdrawal stops that host's control link and waits for it
/// to be gone (spec 7.1): a withdrawal that has answered has nothing left
/// dialing that host.
///
/// Watched as the host seeing its control connection released, because a link
/// that outlived its enrollment does not misbehave in any way a count of dials
/// or of frames would show. It holds the connection it already has, forever: the
/// host keeps a subscriber it cannot get rid of, the gateway keeps a task
/// writing rows into an entry that is gone, and re-enrolling that address later
/// races the leaked link against the fresh one (see [`Gateway::dial`]).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withdrawal_stops_the_control_link_of_the_host_it_withdraws() {
    // No client attaches here, so this host serves its control connection and
    // nothing else.
    let fake = FakeHost::start("fake", Script::Frames(Vec::new())).await;
    // Enrolled over the wire: a configured host is the file's to remove.
    let fixture = Fixture::new(&[]).await;
    assert_eq!(
        fixture.enroll(&fake.address.to_string()).await.status(),
        StatusCode::OK,
    );
    fixture.until_connected("fake").await;
    let dialed = fake
        .attaches()
        .iter()
        .filter(|attached| attached.is_empty())
        .count();
    assert_eq!(
        (dialed, fake.control_released()),
        (1, 0),
        "one control connection, open: without it there is no link here for a \
         withdrawal to stop and nothing below measures a teardown",
    );

    assert_eq!(
        fixture.withdraw("fake").await.status(),
        StatusCode::NO_CONTENT,
    );

    // The close travels on the link's own connection while the 204 travels on
    // the client's, so the host observes it just after the answer rather than
    // with it. What is being pinned is that it arrives at all.
    bounded(
        "the withdrawn host's control connection to close, which a gateway that \
         has answered a withdrawal is no longer holding",
        async {
            while fake.control_released() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        },
    )
    .await;

    fixture.shutdown().await;
    fake.stop();
}

/// A withdrawal interrupts an in-flight dial to the host being withdrawn, rather
/// than waiting it out (spec 7.1: a withdrawal that has answered has nothing left
/// dialing that host).
///
/// The dials of one client's stream are sequential and each is bounded by the
/// upstream timeout, so a host that takes the request and sits on the response
/// head holds that whole stream open. If the enrollment behind that dial is
/// withdrawn meanwhile, waiting the dial out costs the client every session it
/// holds, on every host: the dial ends in a timeout, a timeout is not a refusal
/// the host made, and the stream request answers 503 for all of them. Racing the
/// dial against the withdrawal instead leaves the withdrawn host contributing no
/// upstream, which is the same thing an unreachable host contributes.
///
/// Two hosts, because that collateral is the point, and the withdrawn one sorts
/// first so its dial is the one in flight when the withdrawal lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withdrawal_interrupts_the_dial_of_the_host_it_withdraws() {
    // Never notified: this host takes the attach and never answers its head.
    let held = Arc::new(tokio::sync::Notify::new());
    let leaving = FakeHost::start(
        "leaving",
        Script::CuedHead {
            cue: Arc::clone(&held),
            frames: block("s-1", "epoch-1", 0),
        },
    )
    .await;
    let staying = FakeHost::start("staying", Script::Frames(block("s-9", "epoch-9", 0))).await;
    let fixture = Fixture::tuned(
        TempDir::new().expect("tempdir"),
        Vec::new(),
        Tuning {
            // Short only so that the failure this pins is quick: waiting the dial
            // out is what the test is about, not how long the wait is.
            upstream_timeout: Duration::from_secs(3),
            ..tuning()
        },
    )
    .await;
    for host in [&leaving, &staying] {
        assert_eq!(
            fixture.enroll(&host.address.to_string()).await.status(),
            StatusCode::OK,
        );
    }
    fixture.until_connected("leaving").await;
    fixture.until_connected("staying").await;

    let attaching = [attach("leaving:s-1"), attach("staying:s-9")];
    let (stream, withdrawn) = tokio::join!(fixture.client.events(&attaching), async {
        bounded("the withdrawn host's dial to be in flight", async {
            while leaving.spliced_attaches().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            staying.spliced_attaches().is_empty(),
            "the dials had already moved past the host being withdrawn, so \
                 there is no in-flight dial here to interrupt: {:?}",
            staying.spliced_attaches(),
        );
        fixture.withdraw("leaving").await
    },);

    assert_eq!(withdrawn.status(), StatusCode::NO_CONTENT);
    let mut events = stream.unwrap_or_else(|err| {
        panic!(
            "the client's whole stream was refused because this gateway waited \
             out a dial to a host it had already forgotten: {err:?}"
        )
    });
    let carried = carried_until(
        &mut events,
        "the block of the host that stayed",
        |carried| !carried.caught_up.is_empty(),
    )
    .await;
    assert_eq!(
        carried.caught_up,
        vec!["staying:s-9".to_string()],
        "the sessions of the host that stayed are served as usual: {carried:?}",
    );
    assert!(
        carried.resets.is_empty() && carried.events("leaving:s-1") == 0,
        "and the withdrawn host contributes no upstream and no re-attach, the \
         same as a host this gateway cannot reach: {carried:?}",
    );

    drop(events);
    fixture.shutdown().await;
    leaving.stop();
    staying.stop();
}

/// A withdrawal reaches a client that has stopped reading, whose upstream is
/// parked mid-block waiting for room in that client's queue (spec 6.9's pacing).
///
/// That is the one place a teardown signal delivered only where frames are read
/// would never arrive, and it is exactly the client whose upstream costs a host
/// a subscriber it can do nothing about.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withdrawal_ends_the_upstream_of_a_client_that_stopped_reading() {
    let resuming = deep_block("s-1", "epoch-1", 800);
    let deep = resuming.len();
    let fake = FakeHost::start("fake", Script::Frames(resuming)).await;
    let fixture = Fixture::new(&[]).await;
    assert_eq!(
        fixture.enroll(&fake.address.to_string()).await.status(),
        StatusCode::OK,
    );
    fixture.until_connected("fake").await;

    // Attached and then never read from, so every buffer behind the client fills
    // and the task pumping its upstream parks.
    let events = fixture.attach(&[attach("fake:s-1")]).await;
    let stalled = fake.until_stalled().await;
    assert!(
        stalled < deep,
        "the client absorbed {stalled} of {deep} block frames, so it is not \
         stalled at all and this test measures nothing",
    );
    assert_eq!(fake.released(), 0, "and its upstream is open");

    assert_eq!(
        fixture.withdraw("fake").await.status(),
        StatusCode::NO_CONTENT,
    );

    bounded("the withdrawn host's upstream to close", async {
        while fake.released() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    drop(events);
    fixture.shutdown().await;
    fake.stop();
}

/// A withdrawal the gateway cannot write down does not stand, and neither does
/// any part of its teardown: the enrollment is there with the rows it had, and
/// the client watching that host is still being served.
///
/// The order this pins is the reason it holds: the withdrawal is written down
/// before the directory is touched, so a write that fails has nothing to put
/// back and nothing was ever torn down.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withdrawal_that_was_not_recorded_leaves_the_splices_alone() {
    let cue = Arc::new(tokio::sync::Notify::new());
    let fake = FakeHost::start(
        "fake",
        Script::Cued {
            first: block("s-1", "epoch-1", 0),
            cue: Arc::clone(&cue),
            then: vec![warning_frame("s-1", "epoch-1", "still watching")],
        },
    )
    .await;
    let fixture = Fixture::new(&[]).await;
    assert_eq!(
        fixture.enroll(&fake.address.to_string()).await.status(),
        StatusCode::OK,
    );
    fixture.until_connected("fake").await;
    let mut events = fixture.attach(&[attach("fake:s-1")]).await;
    carried_until(&mut events, "the attach block", |carried| {
        !carried.caught_up.is_empty()
    })
    .await;
    fixture.row("fake:s-1").await;

    // A directory where the state file goes, so the write that records the
    // withdrawal cannot land.
    let state = fixture.state.path().join("hosts.json");
    std::fs::remove_file(&state).expect("the recorded enrollment");
    std::fs::create_dir(&state).expect("stage an unwritable state file");
    let (status, _, _) = refusal(fixture.withdraw("fake").await).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        fixture.hosts().await.hosts.len(),
        1,
        "a withdrawal that was not recorded did not happen",
    );
    assert_eq!(
        fixture
            .client
            .sessions()
            .await
            .expect("the merged directory")
            .sessions
            .iter()
            .map(|row| row.id.clone())
            .collect::<Vec<_>>(),
        vec!["fake:s-1".to_string()],
        "with the rows it had: this host says nothing further, so a directory \
         emptied and refilled would stay empty",
    );

    cue.notify_one();
    let carried = carried_until(&mut events, "a frame after the refusal", |carried| {
        carried.events("fake:s-1") > 0
    })
    .await;
    assert_eq!(
        fake.released(),
        0,
        "the splice was torn down for a withdrawal that did not happen: {carried:?}",
    );
    assert!(
        carried.resets.is_empty(),
        "a withdrawal that did not happen asked this client to attach its \
         sessions again: {carried:?}",
    );
    assert!(
        !carried.ended,
        "and its whole stream ended over it: {carried:?}",
    );

    std::fs::remove_dir(&state).expect("unstage");
    fixture.shutdown().await;
    fake.stop();
}

/// The same refusal from the other side: a withdrawal that was not recorded
/// publishes *nothing*, because it never mutated anything.
///
/// The reachability channel is where "put it back exactly as it was" is not
/// enough. A `watch` cannot retract a value a receiver has already read, so a
/// withdrawal that mutated first and rolled back afterwards presents every
/// splice on that host with a down edge and then an up edge for a host that
/// never went anywhere, and an up edge is what a splice answers with a `reset`
/// (spec 7.1, and `a_reattach_while_a_host_is_down_waits_for_it_to_return` for
/// the other half of that chain). Whether a given splice observes it is down to
/// the scheduler, which is why the assertion here is on the channel: the edge is
/// either published or it is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withdrawal_that_was_not_recorded_publishes_nothing() {
    let fake = FakeHost::start("fake", Script::Frames(block("s-1", "epoch-1", 0))).await;
    let fixture = Fixture::new(&[]).await;
    assert_eq!(
        fixture.enroll(&fake.address.to_string()).await.status(),
        StatusCode::OK,
    );
    fixture.until_connected("fake").await;
    fixture.row("fake:s-1").await;

    // Subscribed once the host has settled, and a receiver counts its current
    // value as seen, so any wake from here on is this withdrawal's.
    let directory = &fixture.gateway.inner.directory;
    let reachable = directory.reachable();
    let merged = directory.subscribe();
    assert!(
        reachable.borrow().contains("fake"),
        "the host has to be in the reachable set going in, or a channel that \
         stays quiet says nothing about the edge this is looking for",
    );

    // A directory where the state file goes, so the write that records the
    // withdrawal cannot land.
    let state = fixture.state.path().join("hosts.json");
    std::fs::remove_file(&state).expect("the recorded enrollment");
    std::fs::create_dir(&state).expect("stage an unwritable state file");
    let (status, _, _) = refusal(fixture.withdraw("fake").await).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    assert!(
        !reachable.has_changed().expect("the directory is alive"),
        "a withdrawal that did not happen took the host out of the reachable \
         set and put it back, and every splice on it that saw both edges asks \
         its client to re-attach the sessions it holds there",
    );
    assert!(
        !merged.has_changed().expect("the directory is alive"),
        "and it republished the merged directory twice over a set that never \
         changed (spec 6.8)",
    );

    std::fs::remove_dir(&state).expect("unstage");
    fixture.shutdown().await;
    fake.stop();
}

// ---------------------------------------------------------------------------
// Flow control (spec 6.9)
// ---------------------------------------------------------------------------

/// A client the gateway cannot keep up with is evicted rather than buffered
/// without bound, and the ordinary re-attach puts it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_stops_reading_is_evicted_and_recovers() {
    let fake = FakeHost::start(
        "fake",
        Script::Flood {
            session: "s-1".to_string(),
            opening: block("s-1", "epoch-1", 0),
        },
    )
    .await;
    let fixture = Fixture::tuned(
        TempDir::new().expect("tempdir"),
        vec![fake.address.clone()],
        Tuning {
            outbound_queue: NonZeroUsize::new(2).expect("non-zero"),
            ..tuning()
        },
    )
    .await;
    fixture.until_connected("fake").await;

    // From here the client reads nothing at all, while the host writes without
    // end.
    let mut events = fixture.attach(&[attach("fake:s-1")]).await;
    bounded("the stalled client to be evicted", async {
        while fake.released() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        fake.written() > 2,
        "the host wrote more than the client's queue holds, {} frames",
        fake.written(),
    );

    // An evicted stream ends rather than handing back everything it was owed:
    // what the client missed comes back from the backfill of its re-attach.
    let mut carried = 0;
    bounded("the evicted stream to end", async {
        while (events.recv().await).is_some() {
            carried += 1;
        }
    })
    .await;
    assert!(
        carried < fake.written(),
        "the client was served every frame the host wrote, so nothing bounded it: \
         {carried} of {}",
        fake.written(),
    );

    // Recovery is the ordinary re-attach.
    let mut events = fixture.attach(&[attach("fake:s-1")]).await;
    let block = frames_until(&mut events, "the block of the re-attach", is_caught_up).await;
    assert!(
        named_sessions(&block).contains(&"fake:s-1"),
        "an evicted client attaches again and is served: {block:?}",
    );

    drop(events);
    fixture.shutdown().await;
    fake.stop();
}

/// A session's `error` frame ends its attach block, because it is what the
/// server sent instead of one (spec 6.5). Everything that follows for that
/// session is ordinary live traffic, measured against the client's bound: a
/// client that will not read it is evicted (spec 6.9) rather than pacing the
/// upstream it shares with every other session on that host to a standstill.
///
/// The host here keeps talking about a session it refused, which a correct one
/// does not do. That is the point: what a peer may do to this gateway's flow
/// control must not depend on that peer behaving.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_ends_the_block_it_was_sent_instead_of() {
    let fake = FakeHost::start(
        "fake",
        Script::Flood {
            session: "s-1".to_string(),
            opening: vec![error_frame("s-1", "unknown_session", "no session s-1 here")],
        },
    )
    .await;
    let fixture = Fixture::tuned(
        TempDir::new().expect("tempdir"),
        vec![fake.address.clone()],
        Tuning {
            outbound_queue: NonZeroUsize::new(2).expect("non-zero"),
            ..tuning()
        },
    )
    .await;
    fixture.until_connected("fake").await;

    // From here the client reads nothing at all.
    let events = fixture.attach(&[attach("fake:s-1")]).await;
    bounded(
        "the stalled client to be evicted, which paced frames would never do",
        async {
            while fake.released() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        },
    )
    .await;

    assert!(
        fake.written() > 2,
        "the host wrote no more than the client's queue holds, {} frames, so \
         nothing here ever met the bound",
        fake.written(),
    );

    drop(events);
    fixture.shutdown().await;
    fake.stop();
}

/// An attach block bigger than the client's bound does not evict the client that
/// asked for it (spec 6.9).
///
/// The bound governs live fan-out. A block measured against it would evict on
/// the first big backfill, and the re-attach that followed would do the same
/// again, so a client with a real session could never catch up at all.
///
/// The block has to *reach* the bound for that to be measured at all, and one
/// that fits in the sockets between the host and the client never does: the
/// writer never stalls, the queue never fills, and the test passes with pacing
/// removed. So the client here waits until the host's writes stop with the block
/// unfinished, which is the state where the queue is full of paced frames and the
/// task pumping them is parked. How much fits is the machine's business, so it is
/// checked rather than assumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_block_bigger_than_the_bound_does_not_evict_its_own_client() {
    let backfilled: u64 = 800;
    let resuming = deep_block("s-1", "epoch-1", backfilled);
    let deep = resuming.len();
    let fake = FakeHost::start("fake", Script::Frames(resuming)).await;
    let fixture = Fixture::tuned(
        TempDir::new().expect("tempdir"),
        vec![fake.address.clone()],
        Tuning {
            outbound_queue: NonZeroUsize::new(2).expect("non-zero"),
            ..tuning()
        },
    )
    .await;
    fixture.until_connected("fake").await;

    let mut events = fixture.attach(&[attach("fake:s-1")]).await;
    let stalled = fake.until_stalled().await;
    assert!(
        stalled < deep,
        "the client absorbed {stalled} of {deep} block frames, so its queue never \
         reached the bound and this test measures nothing",
    );
    // An eviction cancels the splice, so the upstream carrying the block goes
    // with it: the loss lands on the host before the client can read a frame of
    // it.
    assert_eq!(
        fake.released(),
        0,
        "the block evicted the client that asked for it, {stalled} frames in",
    );

    let carried = carried_until(&mut events, "the whole attach block", |carried| {
        !carried.caught_up.is_empty()
    })
    .await;

    assert_eq!(
        carried.events("fake:s-1"),
        usize::try_from(backfilled).expect("a count that fits"),
        "every frame of a block hundreds deep arrived against a bound of two: {carried:?}",
    );
    assert_eq!(carried.caught_up, vec!["fake:s-1".to_string()]);

    fixture.shutdown().await;
    fake.stop();
}

/// A live frame from another host does not evict a client mid-block (spec 6.9:
/// "the bound governs live fan-out only"; spec 7.1: a block measured against the
/// bound would evict the very client that asked for it, and the re-attach would
/// do the same again).
///
/// It takes two hosts, which is what a gateway is for: one host's frames are
/// forwarded by one task in order, so a live frame of its own can never arrive
/// while its own block is still in flight. In production this is a client
/// watching a big session resume on one host while a turn runs on another.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_frame_from_another_host_does_not_evict_a_client_mid_block() {
    let backfilled: u64 = 800;
    let resuming = deep_block("s-1", "epoch-1", backfilled);
    let deep = resuming.len();
    let left = FakeHost::start("left", Script::Frames(resuming)).await;
    let cue = Arc::new(tokio::sync::Notify::new());
    let opening = block("s-9", "epoch-9", 0);
    let cued_at = opening.len() + 1;
    let right = FakeHost::start(
        "right",
        Script::Cued {
            first: opening,
            cue: Arc::clone(&cue),
            then: vec![warning_frame("s-9", "epoch-9", "a turn on the other host")],
        },
    )
    .await;
    let fixture = Fixture::tuned(
        TempDir::new().expect("tempdir"),
        vec![left.address.clone(), right.address.clone()],
        Tuning {
            outbound_queue: NonZeroUsize::new(2).expect("non-zero"),
            ..tuning()
        },
    )
    .await;
    fixture.until_connected("left").await;
    fixture.until_connected("right").await;

    // Read until the second session is caught up, so that what arrives for it
    // from here on is live rather than part of its own block.
    let mut events = fixture
        .attach(&[attach("left:s-1"), attach("right:s-9")])
        .await;
    let opened = carried_until(&mut events, "the other host's block", |carried| {
        carried.caught_up.contains(&"right:s-9".to_string())
    })
    .await;

    // From here the client reads nothing, so the queue fills with the paced
    // frames of the block and the task pumping it parks.
    let stalled = left.until_stalled().await;
    assert!(
        stalled < deep,
        "the client absorbed {stalled} of {deep} block frames, so its queue never \
         reached the bound and this test measures nothing",
    );
    cue.notify_one();
    bounded("the live frame to leave the other host", async {
        while right.written() < cued_at {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    tokio::time::sleep(QUIET).await;

    // Read on the far side of the eviction rather than through it: an eviction
    // cancels the whole splice, so the upstream carrying the block is gone with
    // it, and that is where a client's loss lands.
    assert_eq!(
        left.released(),
        0,
        "one live frame from another host evicted the client mid-block, after \
         {stalled} of {deep} frames of the block it had asked for",
    );

    let carried = carried_until(&mut events, "the rest of the block", |carried| {
        carried.caught_up.contains(&"left:s-1".to_string())
    })
    .await;
    assert_eq!(
        opened.events("left:s-1") + carried.events("left:s-1"),
        usize::try_from(backfilled).expect("a count that fits"),
        "the whole block reached the client: {carried:?}",
    );
    assert_eq!(
        opened.events("right:s-9") + carried.events("right:s-9"),
        1,
        "and so did the live frame, queued behind the block rather than into it",
    );

    fixture.shutdown().await;
    left.stop();
    right.stop();
}

/// Two sessions on one host share one upstream: both blocks are paced, and a
/// stream that breaks resets both of them (spec 6.5, 6.3).
///
/// One pump carries every session a client attached on one host, and two pieces
/// of its state are per session: the set still being backfilled, and the `reset`
/// a broken stream owes. Treating either as one session's is invisible with one
/// session attached, which is all any other test here does. The second block is
/// deep, because a session dropped out of the paced set is not evicted until its
/// block meets the live bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sessions_on_one_host_are_both_paced_and_both_reset() {
    let backfilled: u64 = 800;
    let mut script = block("s-1", "epoch-1", 0);
    script.extend(deep_block("s-2", "epoch-2", backfilled));
    let deep = script.len();
    let fake = FakeHost::start("fake", Script::Ends(script)).await;
    let fixture = Fixture::tuned(
        TempDir::new().expect("tempdir"),
        vec![fake.address.clone()],
        Tuning {
            outbound_queue: NonZeroUsize::new(2).expect("non-zero"),
            ..tuning()
        },
    )
    .await;
    fixture.until_connected("fake").await;

    let mut events = fixture
        .attach(&[attach("fake:s-1"), attach("fake:s-2")])
        .await;
    let stalled = fake.until_stalled().await;
    assert!(
        stalled < deep,
        "the client absorbed {stalled} of {deep} frames, so its queue never reached \
         the bound and this test measures nothing",
    );

    let carried = carried_until(&mut events, "both blocks", |carried| {
        carried.caught_up.len() == 2
    })
    .await;
    assert!(
        !carried.ended,
        "the client was evicted mid-block, which is what measuring the second \
         session's block against the live bound does: {carried:?}",
    );
    assert_eq!(
        carried.caught_up,
        vec!["fake:s-1".to_string(), "fake:s-2".to_string()],
    );
    assert_eq!(
        carried.events("fake:s-2"),
        usize::try_from(backfilled).expect("a count that fits"),
        "and the second session's block arrived whole: {carried:?}",
    );

    // The host hangs up once its script runs out, which it can only reach after
    // the client has drained it. Continuity broke for both sessions the stream
    // carried, so both are told.
    let mut named = resets(&frames_within(&mut events, QUIET).await);
    named.sort();
    assert_eq!(
        named,
        vec!["fake:s-1".to_string(), "fake:s-2".to_string()],
        "a lost upstream resets every session it carried",
    );

    fixture.shutdown().await;
    fake.stop();
}

/// No upstream is pumped before every dial is done (spec 7.1: "returning means
/// every upstream that could be opened is open").
///
/// The dials are sequential and each is bounded by `upstream_timeout`, so a pump
/// started inside that loop forwards one host's frames for as long as the
/// remaining dials take, into a queue for a client that has not been handed its
/// response head yet. A busy session there evicts a client that never saw a
/// frame, and its re-attach reproduces the state exactly.
///
/// The queue here is deliberately big enough for the whole block, so what the
/// assertion measures is what the gateway pulled out of the first host and not
/// where a bound bit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_upstream_is_pumped_before_every_dial_is_done() {
    let backfilled: u64 = 800;
    let talking = deep_block("s-1", "epoch-1", backfilled);
    let deep = talking.len();
    let first = FakeHost::start("aaa", Script::Frames(talking)).await;
    let cue = Arc::new(tokio::sync::Notify::new());
    let slow = FakeHost::start(
        "zzz",
        Script::CuedHead {
            cue: Arc::clone(&cue),
            frames: block("s-9", "epoch-9", 0),
        },
    )
    .await;
    let fixture = Fixture::tuned(
        TempDir::new().expect("tempdir"),
        vec![first.address.clone(), slow.address.clone()],
        Tuning {
            outbound_queue: NonZeroUsize::new(deep * 2).expect("non-zero"),
            ..tuning()
        },
    )
    .await;
    fixture.until_connected("aaa").await;
    fixture.until_connected("zzz").await;

    // The stream request cannot come back until the second host answers its
    // head, and that wait is the window under test.
    let client = RemoteClient::new(&fixture.server.url()).expect("client");
    let opening = tokio::spawn(async move {
        client
            .events(&[attach("aaa:s-1"), attach("zzz:s-9")])
            .await
            .expect("a client stream onto the gateway")
    });
    let stalled = first.until_stalled().await;

    assert!(
        stalled < deep,
        "the gateway drained {stalled} of {deep} frames out of the first host \
         while the second was still being dialed, into a queue for a client that \
         had not been handed its response head",
    );

    // And once the second host answers, the client is served both blocks whole.
    cue.notify_one();
    let mut events = bounded("the stream to open", opening)
        .await
        .expect("the task");
    let mut blocks = 0;
    let served = frames_until(&mut events, "both blocks", |frame| {
        if is_caught_up(frame) {
            blocks += 1;
        }
        blocks == 2
    })
    .await;
    assert_eq!(
        served
            .iter()
            .filter(|frame| matches!(frame, Frame::Event { session, .. } if session == "aaa:s-1"))
            .count(),
        usize::try_from(backfilled).expect("a count that fits"),
        "and the first host's block arrived whole behind the wait",
    );

    fixture.shutdown().await;
    first.stop();
    slow.stop();
}

/// A host that takes a stream request and never answers it is a 503, not a hang:
/// a client of a gateway must not be held open for as long as a host cares to
/// stay silent.
///
/// This bounds the response *head* only. Once the stream is open, silence is the
/// client's own business (two missed heartbeats, spec 6.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_that_never_answers_an_attach_becomes_a_503() {
    let fake = FakeHost::start("fake", Script::Mute).await;
    let fixture = Fixture::tuned(
        TempDir::new().expect("tempdir"),
        vec![fake.address.clone()],
        Tuning {
            upstream_timeout: Duration::from_millis(300),
            ..tuning()
        },
    )
    .await;
    fixture.until_connected("fake").await;

    let started = std::time::Instant::now();
    let Err(err) = fixture.client.events(&[attach("fake:s-1")]).await else {
        panic!("a host that answers nothing cannot serve an attach");
    };

    assert_eq!(
        err.status(),
        Some(StatusCode::SERVICE_UNAVAILABLE),
        "got {err:?}",
    );
    assert_eq!(err.code(), Some("host_unreachable"));
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "the splice waited {took:?} on a host it had bounded",
    );

    fixture.shutdown().await;
    fake.stop();
}

/// A client that goes away releases the upstream streams opened for it, so a
/// host stops paying for a subscriber nobody reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_stream_that_ends_releases_its_upstreams() {
    let fake = FakeHost::start("fake", Script::Frames(block("s-1", "epoch-1", 0))).await;
    let fixture = Fixture::over(TempDir::new().expect("tempdir"), vec![fake.address.clone()]).await;
    fixture.until_connected("fake").await;
    let mut events = fixture.attach(&[attach("fake:s-1")]).await;
    frames_until(&mut events, "the attach block", is_caught_up).await;
    assert_eq!(fake.released(), 0, "the client is still reading");

    drop(events);

    bounded("the upstream to be released", async {
        while fake.released() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    fixture.shutdown().await;
    fake.stop();
}

/// A shutdown ends a spliced client's stream rather than waiting for it, and
/// releases the upstreams behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shutdown_ends_a_spliced_stream_and_its_upstreams() {
    let fake = FakeHost::start("fake", Script::Frames(block("s-1", "epoch-1", 0))).await;
    let fixture = Fixture::over(TempDir::new().expect("tempdir"), vec![fake.address.clone()]).await;
    fixture.until_connected("fake").await;
    let mut events = fixture.attach(&[attach("fake:s-1")]).await;
    frames_until(&mut events, "the attach block", is_caught_up).await;

    let started = std::time::Instant::now();
    fixture.shutdown().await;
    let took = started.elapsed();

    assert!(
        took < Duration::from_secs(2),
        "the shutdown waited {took:?} on a client it could have closed",
    );
    assert!(
        bounded("the end of the stream", events.recv())
            .await
            .is_none(),
        "the client is told rather than left holding a stream nothing writes to",
    );
    bounded("the upstream to be released", async {
        while fake.released() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    fake.stop();
}

/// A shutdown releases the upstreams of a client that stopped reading, which is
/// the client that cannot be told.
///
/// Its stream is polled only when there is room to write to it, so it never
/// observes the shutdown token at all. What ends its upstreams is the token
/// behind them: the splice's own, a child of the serving port's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shutdown_releases_a_stalled_clients_upstreams() {
    let resuming = deep_block("s-1", "epoch-1", 800);
    let deep = resuming.len();
    let fake = FakeHost::start("fake", Script::Frames(resuming)).await;
    let fixture = Fixture::over(TempDir::new().expect("tempdir"), vec![fake.address.clone()]).await;
    fixture.until_connected("fake").await;

    // Attached and then never read from, so every buffer behind the client fills
    // and the task pumping its upstream parks.
    let events = fixture.attach(&[attach("fake:s-1")]).await;
    let stalled = fake.until_stalled().await;
    assert!(
        stalled < deep,
        "the client absorbed {stalled} of {deep} block frames, so it is not stalled \
         at all and this test measures nothing",
    );

    // Shut down alongside the wait, because a stalled client's connection costs
    // the whole grace period and the release must not.
    let shutting = tokio::spawn(fixture.shutdown());
    let released = bounded(
        "the shutdown to release the stalled client's upstream",
        async {
            for _ in 0..100 {
                if fake.released() > 0 {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            false
        },
    )
    .await;

    assert!(
        released,
        "the gateway shut down and the host still holds a subscriber for a client \
         that stopped reading",
    );
    bounded("the shutdown to finish", shutting)
        .await
        .expect("the shutdown task");
    drop(events);
    fake.stop();
}

// ---------------------------------------------------------------------------
// Test support for the splice
// ---------------------------------------------------------------------------

/// Wait until `session` is idle with at least `last_seq` durable entries, read
/// off the host itself rather than off the stream under test.
async fn settled(host: &Upstream, session: &str, last_seq: u64) {
    bounded("the turn to land and settle", async {
        loop {
            let list = host.host.sessions().await.expect("the host's directory");
            let row = list
                .sessions
                .iter()
                .find(|row| row.id == session)
                .expect("the session is this host's");
            if !row.working && row.last_seq.unwrap_or(0) >= last_seq {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

fn is_caught_up(frame: &Frame) -> bool {
    matches!(frame, Frame::CaughtUp { .. })
}

/// Read frames in decoded form until `done` accepts one, which is how a test
/// sees a kind this build does not know (spec 6.10).
async fn decoded_until(
    events: &mut RemoteEvents,
    what: &str,
    mut done: impl FnMut(&DecodedFrame) -> bool,
) -> Vec<DecodedFrame> {
    let mut seen = Vec::new();
    bounded(what, async {
        loop {
            let Some(frame) = events.recv_decoded().await else {
                panic!("the stream ended before {what}, having carried {seen:?}");
            };
            let frame = frame.unwrap_or_else(|err| panic!("a good frame: {err}"));
            let stop = done(&frame);
            seen.push(frame);
            if stop {
                return;
            }
        }
    })
    .await;
    seen
}

/// The settings a fake host reports, which no test reads: only its presence in
/// a `state` frame matters here.
fn fake_settings() -> AgentSettings {
    AgentSettings {
        provider: "scripted".into(),
        model_id: "scripted".into(),
        thinking: "off".into(),
        thinking_display: "default".into(),
        speed: "standard".into(),
        verbosity: "default".into(),
    }
}

/// One directory row, as a fake host writes it about its own session.
fn fake_row(id: &str) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        live: true,
        working: false,
        queued: aj_wire::QueueCounts::default(),
        tasks: 0,
        last_seq: Some(1),
        last_activity: chrono::DateTime::UNIX_EPOCH,
        tag: None,
        host: None,
        unreachable: false,
        archived: false,
        locked: false,
        lock_generation: None,
    }
}

/// The frames of one attach block, as a host writes them (spec 6.5).
fn block(session: &str, epoch: &str, last_seq: u64) -> Vec<String> {
    vec![
        state_frame(session, epoch, last_seq),
        caught_up_frame(session, epoch, last_seq),
    ]
}

/// An attach block with `backfilled` frames between its `state` and its
/// `caught_up`: what a resumed session looks like.
///
/// Chunky frames for the same reason [`flood`]'s are: what a client that is not
/// reading is measured against is bytes in flight, so big frames fill the
/// sockets between a host and that client in hundreds of frames rather than in
/// millions. A test that wants the client's queue at its bound waits for the
/// host's writes to stall ([`FakeHost::until_stalled`]) and checks that this
/// block did not fit.
fn deep_block(session: &str, epoch: &str, backfilled: u64) -> Vec<String> {
    let payload = "x".repeat(32768);
    let mut frames = vec![state_frame(session, epoch, backfilled)];
    frames.extend(
        (1..=backfilled).map(|entry| warning_frame(session, epoch, &format!("{entry}:{payload}"))),
    );
    frames.push(caught_up_frame(session, epoch, backfilled));
    frames
}

/// The `state` frame an attach block opens with.
fn state_frame(session: &str, epoch: &str, last_seq: u64) -> String {
    serde_json::to_string(&Frame::State {
        session: session.to_string(),
        epoch: epoch.to_string(),
        working: false,
        settings: fake_settings(),
        last_seq,
    })
    .expect("a state frame")
}

/// The `caught_up` frame an attach block ends with.
fn caught_up_frame(session: &str, epoch: &str, last_seq: u64) -> String {
    serde_json::to_string(&Frame::CaughtUp {
        session: session.to_string(),
        epoch: epoch.to_string(),
        last_seq,
    })
    .expect("a caught_up frame")
}

/// A reliable-transient event frame, which is what a client that stops reading
/// is measured against: it may be neither coalesced nor dropped (spec 6.4).
fn warning_frame(session: &str, epoch: &str, text: &str) -> String {
    serde_json::to_string(&Frame::Event {
        session: session.to_string(),
        epoch: epoch.to_string(),
        durability: None,
        event: AgentEvent::Warning {
            agent_id: AgentId::Main,
            text: text.to_string(),
        }
        .into(),
    })
    .expect("a warning frame")
}

/// The refusal a host writes instead of a session's attach block (spec 6.5).
fn error_frame(session: &str, code: &str, message: &str) -> String {
    serde_json::to_string(&Frame::Error {
        session: session.to_string(),
        epoch: None,
        code: code.to_string(),
        message: message.to_string(),
        lock_generation: None,
    })
    .expect("an error frame")
}

/// What a [`FakeHost`] writes on a stream that attaches something.
#[derive(Clone)]
enum Script {
    /// These frames, then silence with the stream held open.
    Frames(Vec<String>),
    /// An attach block for every session the stream attached, then silence with
    /// the stream held open. What a host that holds whatever it is asked for
    /// looks like, which is what lets one script serve a store whose sessions
    /// changed under it.
    Blocks,
    /// These frames, then the host hangs the stream up. The flap a gateway must
    /// not paper over by dialing again on its own.
    Ends(Vec<String>),
    /// `opening`, then reliable frames without end for `session`.
    Flood {
        session: String,
        /// What the stream opens with: an attach block, or the refusal a server
        /// writes instead of one (spec 6.5).
        opening: Vec<String>,
    },
    /// These frames, then the ones after `cue` is notified, then silence with
    /// the stream held open. For a live frame that has to arrive at a moment the
    /// test chooses: after another host's block has provably filled the client's
    /// queue.
    Cued {
        first: Vec<String>,
        cue: Arc<tokio::sync::Notify>,
        then: Vec<String>,
    },
    /// A response head that waits for `cue`, then these frames. A host on a slow
    /// link, which is what a splice is waiting on while the upstreams it already
    /// opened are open.
    CuedHead {
        cue: Arc<tokio::sync::Notify>,
        frames: Vec<String>,
    },
    /// Not a stream at all: the host's own refusal of the attach.
    Refuse,
    /// A refusal whose body the test writes, for the shapes spec 6.6 admits and
    /// this build has no type for: an envelope carrying only a `message`, fields
    /// a newer host adds to one.
    RefuseRaw(&'static str),
    /// Nothing at all, not even a response head: the host took the request and
    /// went quiet.
    Mute,
}

/// A stand-in host with a scripted event stream.
///
/// For the three things a real host cannot be made to do on demand: send a frame
/// kind from the future, write faster than a client reads, and refuse an attach
/// with a code this gateway has no vocabulary of its own for. It answers `hello`
/// under its id, holds its control connection open with one `list` frame on it,
/// and records the attach parameters of every stream it is asked for.
struct FakeHost {
    address: HostAddress,
    /// The `session` parameters of every stream, in the order they arrived. A
    /// control connection names none, and is recorded as an empty row.
    attaches: Arc<StdMutex<Vec<Vec<String>>>>,
    /// How many spliced streams have been released, which is how a test sees an
    /// upstream connection being closed from the gateway's side.
    released: Arc<AtomicUsize>,
    /// The same for this host's control connections, counted apart because the
    /// two are torn down by different things: a splice ends with the client
    /// stream it belongs to, the control link only when the gateway stops
    /// following this host.
    control_released: Arc<AtomicUsize>,
    /// How many frames this host's spliced streams have written, scripted and
    /// flooded alike. A count that stops moving is how a test sees the whole
    /// pipeline behind it stall (see [`Self::until_stalled`]).
    written: Arc<AtomicUsize>,
    /// Set for a host a test can replace with a different store at the same
    /// address (see [`Self::rebuildable`]).
    rebuild: Option<Rebuild>,
    /// Set for a host whose control connection resends its directory on cue (see
    /// [`Self::republishing`]).
    republish: Option<Republish>,
    serving: tokio::task::JoinHandle<()>,
}

/// Everything about a fake host that changes when the store behind its address
/// does: the id it answers to, and the directory it publishes.
#[derive(Clone)]
struct Identity {
    host_id: String,
    /// This host's own `list` frame, written out as JSON.
    directory: String,
}

/// What a fake host does when a test rebuilds it: take the new identity, and end
/// the control connection so the gateway's link meets it.
///
/// The identity changes before the connection does, so the redial that drop
/// provokes cannot race the swap and read the old one back.
#[derive(Clone)]
struct Rebuild {
    cue: Arc<tokio::sync::Notify>,
    becomes: Identity,
    identity: Arc<StdMutex<Identity>>,
}

/// What a fake host's control connection does after its opening directory.
enum ControlScript {
    /// Held open and silent, which is what every host but the two below does.
    Held,
    /// Replaced by a different store on cue (see [`FakeHost::rebuildable`]).
    Rebuilt {
        becomes: String,
        /// The rows the rebuilt store publishes, written out as JSON.
        holding: Vec<String>,
    },
    /// One further directory per cue, each as the rows of a `list` frame (see
    /// [`FakeHost::republishing`]).
    Republishes(Vec<Vec<String>>),
}

/// The directories a host's control connection still owes, each waiting for its
/// own cue.
#[derive(Clone)]
struct Republish {
    /// One cue and the `list` frame it releases, in the order they are fired.
    frames: Vec<(Arc<tokio::sync::Notify>, String)>,
}

impl FakeHost {
    async fn start(host_id: &str, script: Script) -> Self {
        Self::with_rows(
            host_id,
            script,
            vec![serde_json::to_string(&fake_row("s-1")).expect("a row")],
        )
        .await
    }

    /// A host [`Self::rebuild`] can replace with a different store at the same
    /// address: from then on it answers `hello` under `becomes` and its directory
    /// is `holding` rather than the `s-1` every other fake host serves.
    ///
    /// What a rebuilt host looks like to a gateway. Its store is a different
    /// store, so its sessions are different sessions, which is what makes the
    /// rows under the new namespace tell a test which store they came from. The
    /// control connection ends as the id changes, so the link redials and meets
    /// the new one, while the streams spliced onto this host are untouched: that
    /// is what leaves a client still attached under the old identity when the
    /// change lands, which is the only moment a `reset` for it can be observed.
    async fn rebuildable(host_id: &str, becomes: &str, holding: &str, script: Script) -> Self {
        Self::built(
            host_id,
            script,
            vec![serde_json::to_string(&fake_row("s-1")).expect("a row")],
            ControlScript::Rebuilt {
                becomes: becomes.to_string(),
                holding: vec![serde_json::to_string(&fake_row(holding)).expect("a row")],
            },
        )
        .await
    }

    /// A host whose control connection publishes `rows`, and then one further
    /// directory per entry of `then`, each waiting for its own
    /// [`Self::republish`].
    ///
    /// What a host resending its `list` looks like, which is ordinary: a host
    /// publishes its directory as its own clients come and go. One per cue rather
    /// than all at once, because a gateway that turns an unchanged snapshot into
    /// a frame downstream is only observable while its clients are idle between
    /// two of them.
    async fn republishing(host_id: &str, rows: Vec<String>, then: Vec<Vec<String>>) -> Self {
        Self::built(
            host_id,
            Script::Frames(Vec::new()),
            rows,
            ControlScript::Republishes(then),
        )
        .await
    }

    /// The same, with the directory this host publishes on its control
    /// connection written out as JSON.
    ///
    /// Raw rather than typed, because what a gateway must not reshape is the
    /// bytes a host wrote: a row from a host a version ahead carries fields this
    /// build has no type for, and number literals a re-encode would round.
    async fn with_rows(host_id: &str, script: Script, rows: Vec<String>) -> Self {
        Self::built(host_id, script, rows, ControlScript::Held).await
    }

    async fn built(
        host_id: &str,
        script: Script,
        rows: Vec<String>,
        control_script: ControlScript,
    ) -> Self {
        use axum::extract::Query;
        use axum::response::IntoResponse;
        use axum::response::sse::{Event, Sse};
        use axum::routing::get;

        let attaches: Arc<StdMutex<Vec<Vec<String>>>> = Arc::new(StdMutex::new(Vec::new()));
        let released = Arc::new(AtomicUsize::new(0));
        let control_released = Arc::new(AtomicUsize::new(0));
        let written = Arc::new(AtomicUsize::new(0));
        // Read per request rather than baked into the responses, because a
        // rebuild changes it under a gateway that is already following this host.
        let identity = Arc::new(StdMutex::new(Identity {
            host_id: host_id.to_string(),
            directory: directory_frame(&rows),
        }));
        let rebuild = match &control_script {
            ControlScript::Rebuilt { becomes, holding } => Some(Rebuild {
                cue: Arc::new(tokio::sync::Notify::new()),
                becomes: Identity {
                    host_id: becomes.clone(),
                    directory: directory_frame(holding),
                },
                identity: Arc::clone(&identity),
            }),
            ControlScript::Held | ControlScript::Republishes(_) => None,
        };
        let republish = match &control_script {
            ControlScript::Republishes(directories) => Some(Republish {
                frames: directories
                    .iter()
                    .map(|rows| (Arc::new(tokio::sync::Notify::new()), directory_frame(rows)))
                    .collect(),
            }),
            ControlScript::Held | ControlScript::Rebuilt { .. } => None,
        };
        let app = axum::Router::new()
            .route(
                "/v1/hello",
                get({
                    let identity = Arc::clone(&identity);
                    move || {
                        let host_id = identity
                            .lock()
                            .expect("the identity mutex is poisoned")
                            .host_id
                            .clone();
                        async move {
                            axum::Json(serde_json::json!({
                                "protocol": PROTOCOL_VERSION,
                                "capabilities": [],
                                "app_version": "0",
                                "host_id": host_id,
                            }))
                        }
                    }
                }),
            )
            .route(
                "/v1/events",
                get({
                    let attaches = Arc::clone(&attaches);
                    let released = Arc::clone(&released);
                    let control_released = Arc::clone(&control_released);
                    let written = Arc::clone(&written);
                    let rebuild = rebuild.clone();
                    let republish = republish.clone();
                    let identity = Arc::clone(&identity);
                    move |Query(params): Query<Vec<(String, String)>>| {
                        let attaches = Arc::clone(&attaches);
                        let released = Arc::clone(&released);
                        let control_released = Arc::clone(&control_released);
                        let written = Arc::clone(&written);
                        let script = script.clone();
                        let rebuild = rebuild.clone();
                        let republish = republish.clone();
                        let identity = Arc::clone(&identity);
                        async move {
                            let attached: Vec<String> = params
                                .iter()
                                .filter(|(key, _)| key == "session")
                                .map(|(_, value)| value.clone())
                                .collect();
                            let control = attached.is_empty();
                            let spliced = {
                                let mut held =
                                    attaches.lock().expect("the attaches mutex is poisoned");
                                held.push(attached.clone());
                                held.iter().filter(|attached| !attached.is_empty()).count()
                            };
                            if !control && matches!(script, Script::Refuse) {
                                return (
                                    StatusCode::CONFLICT,
                                    axum::Json(serde_json::json!({
                                        "code": "locked",
                                        "message": "the session is held by another writer",
                                    })),
                                )
                                    .into_response();
                            }
                            if !control && let Script::RefuseRaw(body) = &script {
                                return (
                                    StatusCode::CONFLICT,
                                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                                    *body,
                                )
                                    .into_response();
                            }
                            if !control && matches!(script, Script::Mute) {
                                std::future::pending::<()>().await;
                            }
                            // Awaited before the response is composed, so the
                            // head itself is what waits.
                            if !control && let Script::CuedHead { cue, .. } = &script {
                                cue.notified().await;
                            }
                            // A control connection carries this host's directory
                            // and then stays open, which is what keeps the
                            // gateway's link to it up. Guarded like a spliced
                            // stream, so a test can watch the gateway let go of
                            // it.
                            let (frames, tail, guard) = if control {
                                (
                                    vec![
                                        identity
                                            .lock()
                                            .expect("the identity mutex is poisoned")
                                            .directory
                                            .clone(),
                                    ],
                                    match rebuild {
                                        Some(rebuild) => Tail::Rebuilt(rebuild),
                                        None => match republish {
                                            Some(republish) => Tail::Republishes(republish),
                                            None => Tail::Held,
                                        },
                                    },
                                    Some(Released(Arc::clone(&control_released))),
                                )
                            } else {
                                let guard = Some(Released(Arc::clone(&released)));
                                match &script {
                                    Script::Frames(frames) => (frames.clone(), Tail::Held, guard),
                                    Script::Blocks => (
                                        attached
                                            .iter()
                                            .flat_map(|session| block(session, "epoch-1", 0))
                                            .collect(),
                                        Tail::Held,
                                        guard,
                                    ),
                                    Script::Ends(frames) => (frames.clone(), Tail::Ended, guard),
                                    Script::Flood { session, opening } => (
                                        opening.clone(),
                                        // Only the first stream floods. A client
                                        // that re-attached into an unending one
                                        // would simply be evicted again, which is
                                        // not what recovery means.
                                        if spliced == 1 {
                                            Tail::Flood(session.clone())
                                        } else {
                                            Tail::Held
                                        },
                                        guard,
                                    ),
                                    Script::Refuse | Script::RefuseRaw(_) | Script::Mute => {
                                        unreachable!("answered above")
                                    }
                                    Script::Cued { first, cue, then } => (
                                        first.clone(),
                                        Tail::Cued(Arc::clone(cue), then.clone()),
                                        guard,
                                    ),
                                    Script::CuedHead { frames, .. } => {
                                        (frames.clone(), Tail::Held, guard)
                                    }
                                }
                            };
                            // Counted as they go out rather than as they were
                            // scripted, so a test can watch this host's writes
                            // stop moving. A control connection's own `list` is
                            // not one of them: what a test measures is the
                            // stream a client is waiting on.
                            let counted = Arc::clone(&written);
                            let opening = futures::StreamExt::map(
                                futures::stream::iter(frames),
                                move |data| {
                                    if !control {
                                        counted.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Ok::<_, std::convert::Infallible>(Event::default().data(data))
                                },
                            );
                            let tail: Pin<
                                Box<
                                    dyn futures::Stream<
                                            Item = Result<Event, std::convert::Infallible>,
                                        > + Send,
                                >,
                            > = match tail {
                                Tail::Flood(session) => Box::pin(flood(session, written, guard)),
                                Tail::Cued(cue, frames) => {
                                    Box::pin(cued(cue, frames, written, guard))
                                }
                                Tail::Held => Box::pin(held(guard)),
                                Tail::Rebuilt(rebuild) => Box::pin(rebuilt(rebuild, guard)),
                                Tail::Republishes(republish) => {
                                    Box::pin(republished(republish, guard))
                                }
                                Tail::Ended => {
                                    drop(guard);
                                    Box::pin(futures::stream::empty())
                                }
                            };
                            Sse::new(futures::StreamExt::chain(opening, tail)).into_response()
                        }
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
        Self {
            address: HostAddress::parse(&format!("http://{bound}")).expect("an address"),
            attaches,
            released,
            control_released,
            written,
            rebuild,
            republish,
            serving,
        }
    }

    /// Publish the `nth` further directory this host was built with, from the
    /// control connection that is up.
    fn republish(&self, nth: usize) {
        self.republish
            .as_ref()
            .expect("only a host built to republish can")
            .frames[nth]
            .0
            .notify_one();
    }

    /// Replace the store behind this address: the id this host answers to
    /// changes, and its control connection ends so the gateway's link meets the
    /// new one.
    fn rebuild(&self) {
        self.rebuild
            .as_ref()
            .expect("only a host built to be rebuilt can be")
            .cue
            .notify_one();
    }

    /// Every stream this host was asked for, control connections included.
    fn attaches(&self) -> Vec<Vec<String>> {
        self.attaches
            .lock()
            .expect("the attaches mutex is poisoned")
            .clone()
    }

    /// Every stream that named a session, which is what a splice opens.
    fn spliced_attaches(&self) -> Vec<Vec<String>> {
        self.attaches()
            .into_iter()
            .filter(|attached| !attached.is_empty())
            .collect()
    }

    fn released(&self) -> usize {
        self.released.load(Ordering::Relaxed)
    }

    /// How many control connections this host has had closed on it.
    fn control_released(&self) -> usize {
        self.control_released.load(Ordering::Relaxed)
    }

    fn written(&self) -> usize {
        self.written.load(Ordering::Relaxed)
    }

    /// Wait until this host stops writing, answering how many frames got out.
    ///
    /// A host whose writes have stopped is one every buffer behind which is
    /// full: the client is not reading, the gateway's queue is at its bound, and
    /// the task pumping this stream is parked. Callers compare the answer with
    /// the script they wrote, because a host that got its whole script out
    /// reached no bound at all and the test that assumed it did would be
    /// measuring the machine's socket buffers.
    async fn until_stalled(&self) -> usize {
        bounded("this host's writes to stall", async {
            let mut last = 0;
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let now = self.written();
                if now > 0 && now == last {
                    return now;
                }
                last = now;
            }
        })
        .await
    }

    fn stop(self) {
        self.serving.abort();
    }
}

/// Counts one released stream when it is dropped, which happens when the
/// response is.
struct Released(Arc<AtomicUsize>);

impl Drop for Released {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// What a fake host's stream does once its scripted frames are written.
enum Tail {
    /// Held open and silent, which is what keeps a gateway's link to it up.
    Held,
    /// Ended, which is what a host hanging up looks like.
    Ended,
    /// Reliable frames without end, for the session named.
    Flood(String),
    /// A wait for the cue, then these frames, then held open.
    Cued(Arc<tokio::sync::Notify>, Vec<String>),
    /// A wait for the rebuild cue, at which point this host takes its new id and
    /// the stream ends. Only a control connection gets one, and only the one
    /// that is up when the cue fires: the redial after it meets a host whose id
    /// has already changed.
    Rebuilt(Rebuild),
    /// One further directory per cue, then held open. Only a control connection
    /// gets one.
    Republishes(Republish),
}

/// A control connection that writes one further directory per cue, in order, and
/// is then held open.
///
/// The cue is awaited before the frame is composed, so a test that fires it
/// before this stream is polled loses nothing: a notification with no waiter is
/// stored.
fn republished(
    republish: Republish,
    guard: Option<Released>,
) -> impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>> {
    let cued = futures::StreamExt::then(
        futures::stream::iter(republish.frames),
        |(cue, frame)| async move {
            cue.notified().await;
            Ok(axum::response::sse::Event::default().data(frame))
        },
    );
    futures::StreamExt::chain(cued, held(guard))
}

/// A stream held open until this host is rebuilt, which ends it.
fn rebuilt(
    rebuild: Rebuild,
    guard: Option<Released>,
) -> impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>> {
    futures::StreamExt::flatten(futures::stream::once(async move {
        rebuild.cue.notified().await;
        *rebuild
            .identity
            .lock()
            .expect("the identity mutex is poisoned") = rebuild.becomes;
        drop(guard);
        futures::stream::empty()
    }))
}

/// One host's directory as it travels on its control connection.
fn directory_frame(rows: &[String]) -> String {
    format!(r#"{{"kind":"list","sessions":[{}]}}"#, rows.join(","))
}

/// A wait for `cue`, then `frames`, then silence with the stream held open.
fn cued(
    cue: Arc<tokio::sync::Notify>,
    frames: Vec<String>,
    written: Arc<AtomicUsize>,
    guard: Option<Released>,
) -> impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>> {
    let cued = futures::StreamExt::flatten(futures::stream::once(async move {
        cue.notified().await;
        futures::stream::iter(frames.into_iter().map(move |data| {
            written.fetch_add(1, Ordering::Relaxed);
            Ok(axum::response::sse::Event::default().data(data))
        }))
    }));
    futures::StreamExt::chain(cued, held(guard))
}

/// A stream that never yields and holds `guard` until it is dropped.
fn held(
    guard: Option<Released>,
) -> impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>> {
    futures::stream::unfold(guard, |guard| async move {
        std::future::pending::<()>().await;
        Some((Ok(axum::response::sse::Event::default()), guard))
    })
}

/// Reliable frames without end, counted as they are written.
///
/// Chunky on purpose: what a stalled client is measured against is bytes in
/// flight, so frames big enough to fill a socket buffer make the bound bite in
/// hundreds of frames rather than in millions.
fn flood(
    session: String,
    written: Arc<AtomicUsize>,
    guard: Option<Released>,
) -> impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>> {
    let payload = "x".repeat(4096);
    futures::stream::unfold((0usize, guard), move |(count, guard)| {
        let frame = warning_frame(&session, "epoch-1", &format!("{count}:{payload}"));
        let written = Arc::clone(&written);
        async move {
            written.fetch_add(1, Ordering::Relaxed);
            tokio::task::yield_now().await;
            Some((
                Ok(axum::response::sse::Event::default().data(frame)),
                (count + 1, guard),
            ))
        }
    })
}

// ---------------------------------------------------------------------------
// The deliberate refusals of this stage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_endpoint_answers_404() {
    let fixture = Fixture::new(&[]).await;

    let response = fixture
        .http
        .get(format!("{}/v1/vms", fixture.server.url()))
        .send()
        .await
        .expect("the request");

    let (status, code, _) = refusal(response).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        code, "unknown_endpoint",
        "probing an endpoint is a valid capability check (spec 6.10)",
    );

    fixture.shutdown().await;
}

// ---------------------------------------------------------------------------
// A host this build does not fully understand (spec 6.10)
// ---------------------------------------------------------------------------

/// A frame kind the gateway cannot read must not cost it the connection: the
/// list frame behind it still reaches the directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_frame_kind_the_gateway_does_not_know_does_not_break_its_link() {
    let (url, serving) = canned_server(
        serde_json::json!({"protocol": PROTOCOL_VERSION, "capabilities": [],
                           "app_version": "9.9.9", "host_id": "canned"}),
        vec![
            r#"{"kind":"something_newer","session":"s","payload":{"a":1}}"#.to_string(),
            r#"{"kind":"list","sessions":[{"id":"2026-01-01-00-00-00-000","live":true,
                "working":false,"queued":{"steering":0,"follow_up":0},"tasks":0,
                "last_seq":3,"last_activity":"2026-01-01T00:00:00Z"}]}"#
                .to_string(),
        ],
    )
    .await;
    let fixture = Fixture::new(&[]).await;
    assert_eq!(fixture.enroll(&url).await.status(), StatusCode::OK);

    let row = fixture.row("canned:2026-01-01-00-00-00-000").await;
    assert_eq!(row.host.as_deref(), Some("canned"));
    assert_eq!(row.last_seq, Some(3), "the host's own row travels through");

    fixture.shutdown().await;
    serving.abort();
}

/// A host that takes a request and never answers it is unreachable, not a hang:
/// a client of a gateway must not be held open for as long as a host cares to
/// stay silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_that_answers_nothing_becomes_a_503() {
    let (url, serving) = wedged_host().await;
    let state = TempDir::new().expect("tempdir");
    let fixture = Fixture::tuned(
        state,
        Vec::new(),
        Tuning {
            upstream_timeout: Duration::from_millis(300),
            ..tuning()
        },
    )
    .await;
    assert_eq!(fixture.enroll(&url).await.status(), StatusCode::OK);
    fixture.row("wedged:2026-01-01-00-00-00-000").await;

    let started = std::time::Instant::now();
    let err = fixture
        .client
        .tree("wedged:2026-01-01-00-00-00-000")
        .await
        .expect_err("the host never answers");

    assert_eq!(
        err.status(),
        Some(StatusCode::SERVICE_UNAVAILABLE),
        "got {err:?}",
    );
    assert_eq!(err.code(), Some("host_unreachable"));
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "the proxy waited {took:?} on a host it had bounded",
    );

    fixture.shutdown().await;
    serving.abort();
}

/// A host that answers the handshake and the control stream, and then nothing
/// at all: every other route hangs for as long as the connection lasts.
async fn wedged_host() -> (String, tokio::task::JoinHandle<()>) {
    use axum::response::sse::{Event, Sse};
    use axum::routing::get;
    use futures::StreamExt;

    let hello = serde_json::json!({"protocol": PROTOCOL_VERSION, "capabilities": [],
                                   "app_version": "0", "host_id": "wedged"});
    let list = r#"{"kind":"list","sessions":[{"id":"2026-01-01-00-00-00-000","live":true,
        "working":false,"queued":{"steering":0,"follow_up":0},"tasks":0,
        "last_activity":"2026-01-01T00:00:00Z"}]}"#;
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
            get(move || async move {
                // One directory, then the stream stays open and silent, which is
                // what keeps the gateway thinking this host is there.
                let frames = futures::stream::iter([Ok::<_, std::convert::Infallible>(
                    Event::default().data(list),
                )])
                .chain(futures::stream::pending());
                Sse::new(frames)
            }),
        )
        .fallback(|| async {
            std::future::pending::<()>().await;
            StatusCode::IM_A_TEAPOT
        });
    let listener = tokio::net::TcpListener::bind(addr("127.0.0.1:0"))
        .await
        .expect("bind");
    let bound = listener.local_addr().expect("local addr");
    let serving = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{bound}"), serving)
}

/// A host that hangs up as soon as its stream opens is backed off like any other
/// failure. Resetting the delay on every connection that *opened* would redial a
/// host in that state at the floor rate for as long as it stayed there.
///
/// The signal is a rate, so the two tunings are set far apart: a floor of 5ms
/// against a ceiling of 200ms means an unbacked-off link dials some tens of times
/// in the window and a backed-off one about a dozen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_that_hangs_up_at_once_is_not_redialed_at_the_floor_rate() {
    let dials = Arc::new(AtomicUsize::new(0));
    let (url, serving) = hanging_up_host(Arc::clone(&dials)).await;
    let fixture = Fixture::tuned(
        TempDir::new().expect("tempdir"),
        Vec::new(),
        Tuning {
            reconnect_delay: Duration::from_millis(5),
            max_reconnect_delay: Duration::from_millis(200),
            ..tuning()
        },
    )
    .await;
    assert_eq!(fixture.enroll(&url).await.status(), StatusCode::OK);

    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let dialled = dials.load(Ordering::Relaxed);

    assert!(dialled > 1, "the link did keep trying, {dialled} times");
    assert!(
        dialled < 20,
        "the link redialled {dialled} times in two seconds, which is the floor rate",
    );

    fixture.shutdown().await;
    serving.abort();
}

/// A host that answers the handshake, opens the stream and closes it at once,
/// counting the handshakes it was asked for.
async fn hanging_up_host(dials: Arc<AtomicUsize>) -> (String, tokio::task::JoinHandle<()>) {
    use axum::response::sse::{Event, Sse};
    use axum::routing::get;

    let hello = serde_json::json!({"protocol": PROTOCOL_VERSION, "capabilities": [],
                                   "app_version": "0", "host_id": "flapping"});
    let app = axum::Router::new()
        .route(
            "/v1/hello",
            get(move || {
                let hello = hello.clone();
                let dials = Arc::clone(&dials);
                async move {
                    dials.fetch_add(1, Ordering::Relaxed);
                    axum::Json(hello)
                }
            }),
        )
        .route(
            "/v1/events",
            get(|| async move {
                Sse::new(futures::stream::empty::<
                    Result<Event, std::convert::Infallible>,
                >())
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

// ---------------------------------------------------------------------------
// The composed path: CLI arguments, a configuration file, a real socket
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_subcommand_serves_the_hosts_its_configuration_names() {
    let mut host = Upstream::start().await;
    let session = host.create().await;
    let dir = TempDir::new().expect("tempdir");
    let config = dir.path().join("gateway.toml");
    std::fs::write(&config, format!("hosts = [\"{}\"]\n", host.address()))
        .expect("write the configuration");
    let args = Args::try_parse_from([
        "aj",
        "--listen=127.0.0.1:0",
        "gateway",
        "--config",
        config.to_str().expect("utf-8 path"),
    ])
    .expect("the gateway arguments parse");
    let Some(aj_app::cli::args::Command::Gateway { config: named }) = &args.command else {
        panic!("the subcommand carries its configuration path");
    };

    let (gateway, server) = start(
        &args,
        config_path(named.as_deref(), dir.path().join("absent.toml"))
            .expect("the named file")
            .as_deref(),
        dir.path().join("state"),
    )
    .await
    .expect("the gateway starts");
    let client = RemoteClient::new(&server.url()).expect("client");

    let id = host.namespaced(&session);
    bounded("the configured host's session", async {
        loop {
            let list = client.sessions().await.expect("the merged directory");
            if list.sessions.iter().any(|row| row.id == id) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    server.shutdown().await;
    gateway.shutdown().await;
    host.stop().await;
}

#[test]
fn the_config_flag_names_a_file_that_has_to_exist() {
    let dir = TempDir::new().expect("tempdir");
    let named = dir.path().join("named.toml");
    let default = dir.path().join("gateway.toml");

    assert_eq!(
        config_path(None, default.clone()).expect("no file is no configuration"),
        None,
        "a gateway with no file serves whatever it was told dynamically",
    );
    assert!(
        config_path(Some(&named), default.clone()).is_err(),
        "a file the operator named has to be there",
    );

    std::fs::write(&named, "hosts = []\n").expect("write");
    std::fs::write(&default, "hosts = []\n").expect("write");
    assert_eq!(
        config_path(Some(&named), default.clone()).expect("the named file"),
        Some(named),
    );
    assert_eq!(
        config_path(None, default.clone()).expect("the default file"),
        Some(default),
    );
}

/// The gate sits outside the routes, so a peer it refuses cannot even learn
/// which endpoints a gateway has (spec 6.11).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_peer_reaches_nothing_on_a_gateway() {
    let state = TempDir::new().expect("tempdir");
    let gateway = Gateway::new(GatewaySetup {
        state_dir: state.path().to_path_buf(),
        static_hosts: Vec::new(),
        tuning: tuning(),
    })
    .expect("a gateway");
    // A gate whose lookups fail refuses every peer, loopback included, which is
    // the only way to be refused from this side of a test.
    let server = GatewayServer::bind(
        gateway.clone(),
        addr("127.0.0.1:0"),
        IdentityGate::tailscale([], FakeWhois::failing()),
    )
    .await
    .expect("bind");
    let http = reqwest::Client::new();

    for route in [
        "/v1/hello",
        "/v1/sessions",
        "/v1/hosts",
        "/v1/events",
        "/v1/nope",
    ] {
        let response = http
            .get(format!("{}{route}", server.url()))
            .send()
            .await
            .expect("the request");
        let (status, code, message) = refusal(response).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{route}");
        assert_eq!(code, "forbidden", "{route}");
        assert_eq!(
            message, "this peer is not authorized",
            "a refused peer learns that it was refused, not why",
        );
    }

    server.shutdown().await;
    gateway.shutdown().await;
}

/// A gateway is remote code execution exactly as a host is, so the identity
/// gate's bind rule applies to it too (spec 6.11).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_gateway_refuses_to_serve_a_public_address() {
    let state = TempDir::new().expect("tempdir");
    let gateway = Gateway::new(GatewaySetup {
        state_dir: state.path().to_path_buf(),
        static_hosts: Vec::new(),
        tuning: tuning(),
    })
    .expect("a gateway");

    let bound =
        GatewayServer::bind(gateway.clone(), addr("0.0.0.0:6161"), IdentityGate::local()).await;
    let Err(err) = bound else {
        panic!("local mode protects nothing on a public address");
    };
    assert!(err.to_string().contains("0.0.0.0"), "got {err}");

    gateway.shutdown().await;
}

/// A dot segment in the session half never reaches a host as a path.
///
/// `<host>:..` names no session, and a URL path drops a `..` segment rather
/// than escaping it, so a proxy that passed one through would address
/// `/v1/sessions` on the host. A POST there is a create, which is the one
/// request this gateway refuses, so the walk would route around the refusal
/// and mint a session.
///
/// `SessionAddress::parse` refuses the shape and a unit test covers that. This
/// test exists because a correct parser proves nothing about the parser being
/// on the request path: it drives the real router over a real socket and reads
/// the outcome off the host's own store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dot_segment_never_reaches_a_host_as_a_path() {
    let mut upstream = Upstream::start().await;
    let fixture = Fixture::new(&[&upstream]).await;
    fixture
        .until("the host's rows", |list| Some(list.sessions.clone()))
        .await;

    let before = upstream
        .host
        .sessions()
        .await
        .expect("the host's directory")
        .sessions
        .len();

    let response = fixture
        .http
        .post(format!(
            "{}/v1/sessions/{}:..",
            fixture.server.url(),
            upstream.host_id()
        ))
        .send()
        .await
        .expect("the traversal request");
    // The store is read before the response is decoded, so a walk that got
    // through fails on the harm it did rather than on the shape of a body the
    // gateway never meant to send.
    let status = response.status();
    let after = upstream
        .host
        .sessions()
        .await
        .expect("the host's directory")
        .sessions
        .len();
    assert_eq!(
        before, after,
        "a dot segment reached the host's create route and minted a session",
    );

    let (_, code, _) = refusal(response).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "code {code}");
    assert_eq!(code, "unknown_session");

    fixture.shutdown().await;
    upstream.stop().await;
}

/// A command for a host the gateway holds no link to is refused before it is
/// forwarded.
///
/// Stopping a host proves less than it looks. Its port closes, so the dial
/// fails and the same `503 host_unreachable` comes back whether or not the
/// gateway consulted its link first, which makes the obvious test vacuous for
/// the check it means to pin. This host answers every route, so the only thing
/// that can refuse the command is the check, and the only thing that can prove
/// the refusal came first is the host having been sent nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_is_refused_before_reaching_a_host_with_no_link() {
    let recorder = Recorder::unlinked("recorder").await;
    let fixture = Fixture::over(
        TempDir::new().expect("tempdir"),
        vec![recorder.address.clone()],
    )
    .await;
    fixture
        .until_hosts("the host to be named and not connected", |hosts| {
            hosts
                .hosts
                .iter()
                .find(|host| host.id.as_deref() == Some("recorder") && !host.connected)
                .map(|_| ())
        })
        .await;

    let outcome = fixture
        .client
        .command("recorder:2026-01-01-00-00-00-000", &prompt("go"))
        .await;

    // The recorder is read before the outcome is unwrapped, so a command that
    // got through fails on having reached the host rather than on the shape of
    // the answer it came back with.
    assert!(
        recorder.proxied().is_empty(),
        "a command reached a host this gateway has no link to: {:?}",
        recorder.proxied(),
    );
    let err = outcome.expect_err("a command for a host with no link is refused");
    assert_eq!(err.status(), Some(StatusCode::SERVICE_UNAVAILABLE));
    assert_eq!(err.code(), Some("host_unreachable"));

    fixture.shutdown().await;
    recorder.stop();
}
