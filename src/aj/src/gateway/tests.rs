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

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use aj_app::cli::args::Args;
use aj_app::host::{AttachRequest, SessionHost};
use aj_app::test_support::finalized_text_message;
use aj_wire::{
    CreateSessionRequest, EnrollHostRequest, ErrorResponse, Frame, HostList, HostSource,
    HostSummary, PROTOCOL_VERSION, PromptInput, PromptRequest, SessionCreated, SessionList,
    SessionSummary,
};
use clap::Parser;
use reqwest::StatusCode;
use tempfile::TempDir;

use super::*;
use crate::gateway::naming::SessionAddress;
use crate::remote::tests::{
    FakeWhois, HostHandles, addr, bounded, canned_server, scripted, scripted_host,
};
use crate::remote::{IdentityGate, RemoteClient, RemoteCommand, RemoteServer};

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
        let dir = TempDir::new().expect("tempdir");
        let (host, server) = Self::serve(&dir, None).await;
        let addr = server.local_addr();
        Self {
            dir,
            host,
            server: Some(server),
            addr,
        }
    }

    /// A host over `dir`'s store, served on `at` or on a fresh loopback port.
    async fn serve(dir: &TempDir, at: Option<SocketAddr>) -> (SessionHost, RemoteServer) {
        let provider = scripted(
            vec![
                finalized_text_message("done"),
                finalized_text_message("done"),
            ],
            0,
            Duration::ZERO,
        );
        let host = scripted_host(dir, provider, HostHandles::new(dir));
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
    /// under the same id and its sessions keep their namespace.
    async fn restart(&mut self) {
        let (host, server) = Self::serve(&self.dir, Some(self.addr)).await;
        self.host = host;
        self.server = Some(server);
    }
}

/// A gateway over a temp state directory, bound on loopback, plus a client for
/// it.
struct Fixture {
    state: TempDir,
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
            static_hosts,
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
            gateway,
            server,
            client,
            http: reqwest::Client::new(),
        }
    }

    /// The same gateway again over the same state directory: a restart, with
    /// nothing but that directory carried across.
    async fn restart(self) -> Self {
        let Self {
            state,
            gateway,
            server,
            ..
        } = self;
        server.shutdown().await;
        gateway.shutdown().await;
        Self::over(state, Vec::new()).await
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
        self.http
            .post(format!("{}/v1/sessions", self.server.url()))
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

    async fn shutdown(self) {
        self.server.shutdown().await;
        self.gateway.shutdown().await;
        drop(self.state);
    }
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

// ---------------------------------------------------------------------------
// A host that is not there (spec 6.8's `unreachable`, 6.1's 503)
// ---------------------------------------------------------------------------

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

    let row = fixture
        .until("the downed host's row to be marked", |list| {
            list.sessions
                .iter()
                .find(|row| row.id == downed && row.unreachable)
                .cloned()
        })
        .await;
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
    fixture
        .until("the returned host's row to clear", |list| {
            list.sessions
                .iter()
                .find(|row| row.id == downed && !row.unreachable)
                .cloned()
        })
        .await;

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

    // A static entry is the configuration's to hold, so it is not persisted: a
    // gateway restarted without it forgets it.
    let fixture = fixture.restart().await;
    assert!(
        fixture.hosts().await.hosts.is_empty(),
        "a static host comes back from the configuration or not at all",
    );

    fixture.shutdown().await;
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
/// configuration is its record from then on (spec 7.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remembered_host_the_configuration_names_too_is_dropped_from_the_state() {
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
        hosts.hosts[0].id, None,
        "a configured host learns its id from the host, so the remembered one \
         cannot outlive a store that changed identity",
    );
    // And it is gone from the state: the file it came from would otherwise
    // resurrect it once the operator removed it from the configuration.
    let recorded =
        std::fs::read_to_string(fixture.state.path().join("hosts.json")).expect("the state file");
    assert!(
        !recorded.contains("remembered"),
        "the remembered enrollment was pruned: {recorded}",
    );

    fixture.shutdown().await;
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
                Frame::List { sessions } => return sessions,
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
                Frame::List { sessions }
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
                Frame::List { sessions } => assert!(sessions.is_empty(), "{sessions:?}"),
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unchanged_directory_publishes_nothing() {
    let mut host = Upstream::start().await;
    let session = host.create().await;
    let fixture = Fixture::new(&[&host]).await;
    fixture.row(&host.namespaced(&session)).await;

    let mut events = fixture.client.events(&[]).await.expect("a stream");
    let first = bounded("the opening directory", async {
        loop {
            if let Frame::List { sessions } =
                events.recv().await.expect("a frame").expect("a good frame")
            {
                return sessions;
            }
        }
    })
    .await;

    // The host republishes its directory as clients come and go, and the
    // gateway must not turn an unchanged payload into a frame downstream.
    match tokio::time::timeout(QUIET, events.recv()).await {
        Err(_) => {}
        Ok(other) => panic!("a settled gateway published {other:?} after {first:?}"),
    }

    fixture.shutdown().await;
    host.stop().await;
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
        .create(r#"{"tag":"fix-auth","added_later":{"n":18446744073709551616}}"#)
        .await;

    assert_eq!(response.status(), StatusCode::OK);
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

/// A stand-in host that keeps the create bodies it is sent.
struct Recorder {
    address: HostAddress,
    creates: Arc<StdMutex<Vec<String>>>,
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
        let app = app.route(
            "/v1/sessions",
            post({
                let creates = Arc::clone(&creates);
                move |body: String| {
                    let creates = Arc::clone(&creates);
                    async move {
                        let mut held = creates.lock().expect("the creates mutex is poisoned");
                        held.push(body);
                        axum::Json(serde_json::json!({"id": format!("recorded-{}", held.len())}))
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
            serving,
        }
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
// The deliberate refusals of this stage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attaching_a_session_is_refused_rather_than_silently_empty() {
    let mut host = Upstream::start().await;
    let session = host.create().await;
    let fixture = Fixture::new(&[&host]).await;
    let id = host.namespaced(&session);
    fixture.row(&id).await;

    let refused = fixture
        .client
        .events(&[AttachRequest {
            session: id,
            cursor: None,
        }])
        .await;
    let Err(err) = refused else {
        panic!("a gateway cannot splice a session stream in this stage");
    };

    assert_eq!(err.status(), Some(StatusCode::CONFLICT), "got {err:?}");
    assert_eq!(err.code(), Some("unsupported"));

    fixture.shutdown().await;
    host.stop().await;
}

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
