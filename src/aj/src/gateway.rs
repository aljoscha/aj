//! `aj gateway`: many session hosts behind one endpoint (spec 7.1).
//!
//! A gateway holds no session logs and no cursors. It keeps one control
//! connection per enrolled host, merges what those hosts say about their
//! sessions into one namespaced directory, and forwards every session request to
//! the host that owns it. Correctness is inherited from the host protocol, which
//! is why this is as thin as it is.
//!
//! The pieces:
//!
//! - [`naming`] is the `<host_id>:<session_id>` grammar, in one place.
//! - [`config`] reads the operator's `gateway.toml`, [`enrollment`] the
//!   gateway's own record of what it was told to keep.
//! - [`directory`] holds the enrolled hosts and composes the merged list.
//! - [`link`] is one host's control connection, dialing until told to stop.
//! - [`splice`] is one client stream: the upstreams of the sessions it
//!   attached, and the `reset` frames a flapping host earns them.
//! - [`outbound`] is that stream's bounded queue (spec 6.9).
//! - [`server`] is the HTTP surface, including the proxy.

mod config;
mod directory;
mod enrollment;
mod link;
mod naming;
mod outbound;
mod server;
mod splice;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

use aj_app::cli::args::{Args, Command, DEFAULT_LISTEN_ADDRESS};
use aj_app::host::AttachRequest;
use aj_wire::{Hello, HostList, HostSource, HostSummary, MergedDirectory, PROTOCOL_VERSION};
use anyhow::{Context, Result, bail};
use reqwest::StatusCode;

use crate::gateway::config::{AddressError, GatewayConfig, HostAddress};
use crate::gateway::directory::{Adopted, Directory, DirectoryError, HostTarget, Route};
use crate::gateway::enrollment::{EnrollmentError, EnrollmentFile};
use crate::gateway::link::Link;
use crate::gateway::server::GatewayServer;
use crate::gateway::splice::Splice;
use crate::remote::{RemoteClient, RemoteError};

/// File under the gateway's state directory holding its own stable id.
///
/// A gateway has no session store to name it, and `GET /v1/hello` carries an id
/// for a gateway as much as for a host (spec 6.1), so it keeps one here beside
/// its enrollments. Persisted rather than minted per process because a client
/// that remembers which endpoint it was talking to should still recognize it
/// after a restart.
const GATEWAY_ID_FILE: &str = "gateway-id";

/// How long a link waits before its first redial, and the ceiling it doubles
/// towards.
///
/// A host on a tailnet that is rebooting is back in seconds, not milliseconds,
/// and an unreachable host costs nothing to leave alone: its sessions are marked
/// and its neighbours are unaffected.
const RECONNECT_DELAY: Duration = Duration::from_millis(500);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(15);

/// How long a client's stream may be idle before a heartbeat frame (spec 6.1).
const HEARTBEAT: Duration = Duration::from_secs(30);

/// How much slack one client's outbound queue holds (spec 6.9).
///
/// The same bound a host applies to its own clients, for the same reason: enough
/// burst room for a client that reads normally, little enough that a client this
/// gateway cannot keep up with is evicted rather than buffered without bound.
const OUTBOUND_QUEUE: NonZeroUsize = NonZeroUsize::new(256).expect("non-zero");

/// How long a proxied request may take before the owning host counts as not
/// answering, and how long establishing that connection may take.
///
/// The same bounds the typed client applies to the same requests: a control port
/// sits on loopback or a tailnet, and every route the proxy carries answers
/// promptly by contract (a command is accepted, not awaited). Without them a host
/// that accepts a connection and then says nothing would hold a client of this
/// gateway open for as long as it cared to.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The timings and bounds a gateway runs on. Tuning, not policy: every value
/// here changes how quickly something is noticed, or how much slack a slow
/// client gets, never what is true.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tuning {
    pub(crate) reconnect_delay: Duration,
    pub(crate) max_reconnect_delay: Duration,
    pub(crate) heartbeat: Duration,
    /// How long a proxied request waits on the owning host.
    pub(crate) upstream_timeout: Duration,
    /// How many frames one client's stream may fall behind by (spec 6.9).
    pub(crate) outbound_queue: NonZeroUsize,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            reconnect_delay: RECONNECT_DELAY,
            max_reconnect_delay: MAX_RECONNECT_DELAY,
            heartbeat: HEARTBEAT,
            upstream_timeout: UPSTREAM_TIMEOUT,
            outbound_queue: OUTBOUND_QUEUE,
        }
    }
}

/// What a gateway is built from.
pub(crate) struct GatewaySetup {
    /// Where the gateway keeps its own state: its id and its enrollments.
    /// `~/.aj/gateway/` in a real run (spec 7.1).
    pub(crate) state_dir: PathBuf,
    /// The hosts the configuration file names, enrolled for as long as it does.
    pub(crate) static_hosts: Vec<HostAddress>,
    pub(crate) tuning: Tuning,
}

/// Why a gateway could not do what was asked of it.
///
/// Typed because the HTTP layer maps the variants onto the status vocabulary of
/// spec 6.1, and because the CLI reports one of them at start-up.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GatewayError {
    #[error(transparent)]
    Address(#[from] AddressError),
    /// The host did not answer, so the gateway does not know its id and cannot
    /// namespace it. 503, which is the one status a gateway has that a host does
    /// not (spec 6.1).
    #[error("could not reach a host at {address}: {source}")]
    Unreachable {
        address: HostAddress,
        #[source]
        source: RemoteError,
    },
    #[error(transparent)]
    Directory(#[from] DirectoryError),
    /// The host that owns an attached session refused it, in its own words: a
    /// session it does not hold, a lock conflict. Carried rather than
    /// interpreted, so a client of a gateway reads a host's refusal exactly as a
    /// client of that host would (spec 6.10).
    #[error("the host answered {status}: {message}")]
    AttachRefused {
        status: StatusCode,
        /// The host whose refusal this is, which is the namespace the session ids
        /// in its body appear under downstream.
        host_id: String,
        /// The host's own sentence, for this gateway's own log and for the
        /// envelope it mints when the host sent none (spec 6.6).
        message: String,
        /// The refusal body as the host wrote it, which is what travels back.
        body: String,
    },
    #[error(transparent)]
    State(#[from] EnrollmentError),
    /// The gateway's own HTTP stack would not start, which only happens before
    /// it serves anything.
    #[error("could not build the gateway's HTTP client: {0}")]
    Http(#[source] reqwest::Error),
}

struct GatewayInner {
    id: String,
    directory: Arc<Directory>,
    /// The enrollment state file, which the dynamic set is written to after
    /// every change to it.
    state: EnrollmentFile,
    /// Held across a change to the enrolled set and the write that records it.
    ///
    /// The directory has its own lock, but it does not cover the file: two
    /// enrollments arriving together would each take a consistent snapshot and
    /// could still write them in the other order, leaving the file one host short
    /// of the directory. That divergence only shows up as a host missing after a
    /// restart, which is the hardest kind of bug to trace back to here.
    writing: TokioMutex<()>,
    /// One per enrolled host. Keyed by address for the same reason the directory
    /// is: the address is what a link dials, and a host that has never answered
    /// has no id to key on.
    links: StdMutex<HashMap<HostAddress, Link>>,
    /// The client the proxy forwards with, shared so that connections to a host
    /// are pooled across requests.
    ///
    /// Deliberately not a [`RemoteClient`]: that one is typed per route, and the
    /// proxy's whole point is to carry a request it does not read.
    http: reqwest::Client,
    tuning: Tuning,
}

/// Every enrolled host, the directory they merge into, and the links keeping it
/// current.
#[derive(Clone)]
pub(crate) struct Gateway {
    inner: Arc<GatewayInner>,
}

impl Gateway {
    /// Build a gateway over `setup`, enrolling its configured and remembered
    /// hosts and dialing all of them.
    ///
    /// A remembered enrollment that the configuration now also names leaves the
    /// state file's enrollments and carries its id across as that configured
    /// host's: the configuration is the record of it being enrolled from here
    /// on, and keeping both would enroll it twice or resurrect it when the
    /// operator removes it from the configuration. Its id is not that record's
    /// to lose, though, because an id names a store (spec 4) and promoting an
    /// enrollment into a file did not change which store answers there.
    ///
    /// A configured host's *id* comes out of the state file too, and is applied
    /// once the enrollments are in place: a host that is down when this gateway
    /// starts is still named by the id its sessions are namespaced under, which
    /// is what a client renders its empty group from (spec 7.1). Applied last so
    /// that a cached id can only ever cost itself: a collision drops the id and
    /// never an enrollment.
    pub(crate) fn new(setup: GatewaySetup) -> Result<Self, GatewayError> {
        let GatewaySetup {
            state_dir,
            static_hosts,
            tuning,
        } = setup;
        let state = EnrollmentFile::new(&state_dir);
        let remembered = state.load()?;
        let directory = Arc::new(Directory::new());
        for address in static_hosts {
            // A host named twice in one configuration is already deduplicated
            // (see `GatewayConfig::hosts`), so anything refused here is a
            // surprise worth saying out loud rather than a routine repeat.
            if let Err(err) = directory.enroll(address.clone(), HostSource::Config, None) {
                tracing::warn!("not enrolling the configured host {address}: {err}");
            }
        }
        let mut pruned = false;
        let mut restoring = remembered.configured_ids;
        for host in remembered.hosts {
            match directory.enroll(
                host.address.clone(),
                HostSource::Dynamic,
                Some(host.host_id.clone()),
            ) {
                Ok(()) => {}
                Err(err) => {
                    // The configuration got there first, which is the ordinary
                    // way this happens: the operator promoted a dynamically
                    // enrolled host into the file. The enrollment is the
                    // configuration's from here on, and the id it answered to is
                    // restored onto it like any other configured host's.
                    tracing::info!("dropping the remembered host {}: {err}", host.address);
                    pruned = true;
                    restoring.push(host);
                }
            }
        }
        for host in restoring {
            // Adopted rather than enrolled, so an id whose address the
            // configuration no longer names brings nothing back with it. That
            // refusal is the ordinary way an entry here dies, and rewriting the
            // file is what stops it coming round again.
            match directory.adopt(&host.address, &host.host_id) {
                Ok(Adopted::Learned | Adopted::Unchanged) => {}
                // Only a state file naming one address twice reaches this, and
                // nothing is spliced onto a gateway that is still being built.
                Ok(Adopted::Replaced(withdrawn)) => withdrawn.end_splices(),
                Err(err) => {
                    tracing::info!(
                        "not restoring the id of the configured host {}: {err}",
                        host.address
                    );
                    pruned = true;
                }
            }
        }
        let inner = Arc::new(GatewayInner {
            id: resolve_gateway_id(&state_dir)?,
            directory,
            state,
            writing: TokioMutex::new(()),
            links: StdMutex::new(HashMap::new()),
            http: proxy_client(tuning.upstream_timeout)?,
            tuning,
        });
        let gateway = Self { inner };
        if pruned {
            gateway.remember()?;
        }
        for address in gateway.inner.directory.addresses() {
            gateway.dial(address);
        }
        Ok(gateway)
    }

    /// Protocol identity and capabilities (spec 6.1).
    ///
    /// No working directory: a gateway serves none of its own, and that absence
    /// is how a client tells the two roles apart. The capability list is empty
    /// for the same reason a host's is, everything the protocol carries today is
    /// in its base version.
    pub(crate) fn hello(&self) -> Hello {
        Hello {
            protocol: PROTOCOL_VERSION,
            capabilities: Vec::new(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            host_id: self.inner.id.clone(),
            working_directory: None,
        }
    }

    /// The merged session directory (spec 7.1).
    pub(crate) fn sessions(&self) -> Arc<MergedDirectory> {
        self.inner.directory.sessions()
    }

    /// The enrolled hosts.
    pub(crate) fn hosts(&self) -> HostList {
        self.inner.directory.hosts()
    }

    /// Enroll the host at `address` and start following it.
    ///
    /// The handshake happens first, and a host that does not answer is refused:
    /// the gateway namespaces a host's sessions under the id that host reports,
    /// and there is nothing to derive that id from but the host itself. So an
    /// enrollment that answered is one the gateway can route and label from this
    /// moment on, restarts included.
    pub(crate) async fn enroll(&self, address: &str) -> Result<HostSummary, GatewayError> {
        let address = HostAddress::parse(address)?;
        let client =
            RemoteClient::new(address.url()).map_err(|source| GatewayError::Unreachable {
                address: address.clone(),
                source,
            })?;
        let hello = client
            .hello()
            .await
            .map_err(|source| GatewayError::Unreachable {
                address: address.clone(),
                source,
            })?;
        // Held across all three, so an enrollment is one step: the file cannot
        // end up describing a set that was never enrolled, and a withdrawal
        // cannot land between the enrolling and the dialing and leave a link
        // behind that nothing will ever stop.
        {
            let _writing = self.inner.writing.lock().await;
            self.inner.directory.enroll(
                address.clone(),
                HostSource::Dynamic,
                Some(hello.host_id.clone()),
            )?;
            if let Err(err) = self.remember() {
                // An enrollment the gateway cannot write down would come back as
                // a surprise absence after a restart, so it does not stand.
                // Nothing to tear down with it: no link was dialed yet, so it
                // was never reachable and no client stream could have spliced
                // onto it.
                let _ = self.inner.directory.withdraw(&hello.host_id);
                return Err(err);
            }
            self.dial(address.clone());
        }
        let hosts = self.hosts();
        hosts
            .hosts
            .into_iter()
            .find(|host| host.address == address.to_string())
            .ok_or_else(|| {
                DirectoryError::UnknownHost {
                    host_id: hello.host_id,
                }
                .into()
            })
    }

    /// Remove the enrollment of `host_id` and tear down what this gateway was
    /// doing for it (spec 7.1).
    ///
    /// Active teardown rather than bookkeeping, in this order:
    ///
    /// 1. the withdrawal is written down, from the set as it would stand without
    ///    that host and having mutated nothing: a withdrawal the gateway cannot
    ///    record does not stand, and one written ahead has nothing to put back;
    /// 2. the enrollment and its rows leave the directory, in one publish, so
    ///    nothing serves a directory that contradicts the enrolled set;
    /// 3. the streams spliced onto that host end, with the `reset` a withdrawal
    ///    owes them: the client re-attaches, the ids it names no longer resolve
    ///    here, and each is refused with its own `error` frame (see
    ///    [`crate::gateway::splice`], which owns that decision);
    /// 4. the control link stops, awaited, so a withdrawal that has answered has
    ///    nothing left dialing that host.
    ///
    /// Steps 3 and 4 both close connections to the host and could be either way
    /// round. The client-visible half goes first.
    ///
    /// Writing first is what keeps a refusal invisible. Mutating first and
    /// putting it back afterwards restores the set exactly and the reachability
    /// watch not at all: a receiver that read the host's absence cannot be told
    /// to forget it, and a splice reads the pair of edges as a host that went
    /// away and returned. It is also the better half of a crash: a process that
    /// dies between 1 and 2 comes back with the host withdrawn, which is what was
    /// asked for.
    pub(crate) async fn withdraw(&self, host_id: &str) -> Result<(), GatewayError> {
        let _writing = self.inner.writing.lock().await;
        let remaining = self.inner.directory.record_without(host_id)?;
        self.inner.state.save(&remaining)?;
        // From here nothing can fail, so nothing needs putting back.
        let (address, withdrawn) = self.inner.directory.withdraw(host_id)?;
        withdrawn.end_splices();
        self.undial(&address).await;
        Ok(())
    }

    /// Where a namespaced session id points, or why it does not.
    pub(crate) fn route(&self, id: &str) -> Result<Route, GatewayError> {
        Ok(self.inner.directory.route(id)?)
    }

    /// Which host a create is for, or why none is (spec 6.6).
    pub(crate) fn create_target(&self, named: Option<&str>) -> Result<HostTarget, GatewayError> {
        Ok(self.inner.directory.create_target(named)?)
    }

    /// Open one client's event stream, splicing every session it attached
    /// (spec 7.1).
    ///
    /// `shutdown` is the serving port's own token: it ends the splice's upstreams
    /// whether or not the client is still reading (see [`Splice::open`]).
    pub(crate) async fn splice(
        &self,
        attach: &[AttachRequest],
        shutdown: &CancellationToken,
    ) -> Result<Splice, GatewayError> {
        // Subscribed before the grouping, so a host that comes up in between is
        // still a change this stream is woken for: the groups are the state the
        // splice compares against, and they are the newer of the two.
        let reachable = self.inner.directory.reachable();
        let plan = self.inner.directory.group(attach);
        Splice::open(
            plan,
            reachable,
            self.inner.directory.subscribe(),
            self.inner.tuning,
            shutdown,
        )
        .await
    }

    /// The client the proxy forwards with.
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.inner.http
    }

    pub(crate) fn tuning(&self) -> Tuning {
        self.inner.tuning
    }

    /// Stop following every host.
    ///
    /// A gateway owns no sessions, no locks and no logs, which is the whole point
    /// of it holding none. The client streams it is serving are the serving
    /// port's: they end, and release the upstreams behind them, on the token
    /// [`GatewayServer::shutdown`] cancels.
    pub(crate) async fn shutdown(&self) {
        let links: Vec<Link> = {
            let mut held = self.links();
            held.drain().map(|(_, link)| link).collect()
        };
        for link in links {
            link.stop().await;
        }
    }

    /// Start a link to `address`, replacing one that is already there.
    fn dial(&self, address: HostAddress) {
        let link = Link::spawn(
            address.clone(),
            Arc::clone(&self.inner.directory),
            self.recorder(),
            self.inner.tuning,
        );
        if let Some(previous) = self.links().insert(address, link) {
            // Only reachable if an address were enrolled twice, which the
            // directory refuses. Stopping the old one anyway keeps a leaked task
            // from writing rows behind the new one's back.
            tokio::spawn(previous.stop());
        }
    }

    fn recorder(&self) -> Recorder {
        Recorder {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Stop the link to `address`, waiting for it to be gone.
    async fn undial(&self, address: &HostAddress) {
        let link = self.links().remove(address);
        if let Some(link) = link {
            link.stop().await;
        }
    }

    /// Write down what this gateway is the record of.
    fn remember(&self) -> Result<(), GatewayError> {
        Ok(self.inner.state.save(&self.inner.directory.record())?)
    }

    fn links(&self) -> std::sync::MutexGuard<'_, HashMap<HostAddress, Link>> {
        self.inner.links.lock().expect("the link mutex is poisoned")
    }
}

/// What a link hands the identity it just learned to (spec 7.1).
///
/// A host id is learned by speaking to the host, so a link is the only thing that
/// can learn one, and a gateway whose hosts all come from the configuration file
/// never enrolls or withdraws anything: an id settled only by those paths would
/// never reach the state file at all, and every restart while such a host is down
/// would come back unable to name it.
///
/// Settling one is a directory change and a write to the gateway's record, in
/// that order and under one lock, which is why it lives here rather than in
/// either of them.
///
/// Weak, because the gateway owns the links this is handed to.
#[derive(Clone)]
pub(crate) struct Recorder {
    inner: Weak<GatewayInner>,
}

impl Recorder {
    /// Settle the id the host at `address` reports, and write it down.
    ///
    /// Write-ahead, the way a withdrawal is: the record is written from the set
    /// as it would stand, and only then does the set change, so a process that
    /// dies in between comes back holding the id its host reported rather than
    /// one this gateway has already stopped serving.
    ///
    /// A write that fails is a log line and nothing more: a recorded id is a
    /// cache for the next run, the host has just answered and its sessions need a
    /// namespace now, and refusing to serve it over a cache write would trade a
    /// working host for a note. That is the opposite of an enrollment, which is
    /// an operator's instruction and does not stand unless it is recorded.
    ///
    /// The teardown a replaced identity leaves behind is finished here, because
    /// what a client is owed for it must not wait on the next redial (see
    /// [`crate::gateway::directory::Withdrawn`]).
    ///
    /// NOTE: this waits on the same lock a withdrawal holds while it awaits the
    /// link's teardown. Not a deadlock, because a link races every attempt
    /// against its own cancellation token, so the withdrawal's `stop` drops this
    /// wait rather than queueing behind it.
    pub(crate) async fn settle(
        &self,
        address: &HostAddress,
        reported: &str,
    ) -> Result<(), DirectoryError> {
        // The gateway this link belonged to is gone, so there is no record to
        // write and nothing reading the directory it would write into.
        let Some(inner) = self.inner.upgrade() else {
            return Ok(());
        };
        let _writing = inner.writing.lock().await;
        let Some(record) = inner.directory.record_adopting(address, reported)? else {
            return Ok(());
        };
        if let Err(err) = inner.state.save(&record) {
            tracing::warn!("could not write down what this gateway just learned: {err}");
        }
        if let Adopted::Replaced(withdrawn) = inner.directory.adopt(address, reported)? {
            tracing::info!("the host at {address} answers to {reported} now");
            withdrawn.end_splices();
        }
        Ok(())
    }
}

/// The client the proxy forwards with.
///
/// Bounded in both directions, because a request this gateway forwards is a
/// client of this gateway waiting: an unbounded one turns a wedged host into a
/// wedged gateway connection, one per request, until something gives up.
fn proxy_client(timeout: Duration) -> Result<reqwest::Client, GatewayError> {
    reqwest::Client::builder()
        .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
        .timeout(timeout)
        .build()
        .map_err(GatewayError::Http)
}

/// The gateway's own id, minted on first use.
///
/// Mirrors a host's `host-id` file, including the `create_new` claim: two
/// gateways sharing a state directory are already a conflict, and this at least
/// keeps them from advertising two ids for one endpoint.
fn resolve_gateway_id(state_dir: &Path) -> Result<String, GatewayError> {
    let path = state_dir.join(GATEWAY_ID_FILE);
    let read = |path: &Path| -> Result<Option<String>, EnrollmentError> {
        match std::fs::read_to_string(path) {
            Ok(id) if !id.trim().is_empty() => Ok(Some(id.trim().to_string())),
            Ok(_) => Ok(None),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(EnrollmentError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    };
    if let Some(id) = read(&path)? {
        return Ok(id);
    }
    std::fs::create_dir_all(state_dir).map_err(|err| EnrollmentError::Write {
        path: state_dir.to_path_buf(),
        reason: err.to_string(),
    })?;
    let minted = format!("{:032x}", rand::random::<u128>());
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
    {
        Ok(mut file) => {
            std::io::Write::write_all(&mut file, format!("{minted}\n").as_bytes()).map_err(
                |err| EnrollmentError::Write {
                    path: path.clone(),
                    reason: err.to_string(),
                },
            )?;
            Ok(minted)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            read(&path)?.ok_or_else(|| {
                // Blank, which only a crash between the create and the write
                // leaves behind. Overwriting it would reopen the race the claim
                // exists to close.
                EnrollmentError::Unusable {
                    path: path.clone(),
                    reason: "the id file is empty: remove it to mint a fresh id".to_string(),
                }
                .into()
            })
        }
        Err(err) => Err(EnrollmentError::Write {
            path: path.clone(),
            reason: err.to_string(),
        }
        .into()),
    }
}

/// `aj gateway`: aggregate the enrolled hosts until the process is asked to
/// stop.
pub(crate) async fn run(args: Args) -> Result<()> {
    let named = match &args.command {
        Some(Command::Gateway { config }) => config.clone(),
        // Only the subcommand routes here.
        _ => None,
    };
    let state_dir = aj_conf::Config::gateway_state_dir_path()
        .context("could not resolve the gateway's state directory")?;
    let config = config_path(
        named.as_deref(),
        aj_conf::Config::gateway_config_file_path()
            .context("could not resolve the gateway's configuration file")?,
    )?;
    let (gateway, server) = start(&args, config.as_deref(), state_dir).await?;

    println!(
        "aj gateway {} serving on {}",
        gateway.hello().host_id,
        server.url()
    );
    crate::serve::wait_for_shutdown().await;

    server.shutdown().await;
    gateway.shutdown().await;
    Ok(())
}

/// The configuration file this run reads, `None` when there is none to read.
///
/// A file the operator named has to exist: the flag is a statement about where
/// the hosts are, and serving none because of a typo is the worst answer to one.
/// The default file is optional, because a gateway that is only ever told about
/// hosts over the wire needs no file at all.
fn config_path(named: Option<&Path>, default: PathBuf) -> Result<Option<PathBuf>> {
    match named {
        Some(path) if path.is_file() => Ok(Some(path.to_path_buf())),
        Some(path) => bail!("--config {}: no such file", path.display()),
        None if default.is_file() => Ok(Some(default)),
        None => Ok(None),
    }
}

/// Compose the gateway `args` asks for and bind its port.
///
/// The listen address defaults to the same loopback control port a bare
/// `--listen` binds, for the same reason `aj serve` defaults it: serving is the
/// point of the mode rather than an addition to it.
async fn start(
    args: &Args,
    config: Option<&Path>,
    state_dir: PathBuf,
) -> Result<(Gateway, GatewayServer)> {
    let listen = args.listen.as_deref().unwrap_or(DEFAULT_LISTEN_ADDRESS);
    let addr = crate::serve::resolve_listen(listen)?;
    let gate = crate::serve::build_gate(args)?;
    let static_hosts = match config {
        Some(path) => GatewayConfig::load(path)?.hosts(),
        None => Vec::new(),
    };
    let gateway = Gateway::new(GatewaySetup {
        state_dir,
        static_hosts,
        tuning: Tuning::default(),
    })
    .context("could not start the gateway")?;
    let server = match GatewayServer::bind(gateway.clone(), addr, gate).await {
        Ok(server) => server,
        Err(err) => {
            gateway.shutdown().await;
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("could not serve the gateway on {addr}"));
        }
    };
    Ok((gateway, server))
}
