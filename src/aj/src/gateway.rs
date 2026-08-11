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
//! - [`server`] is the HTTP surface, including the proxy.
//!
//! Two things this stage deliberately does not do, each refused in one place
//! rather than half-built: splicing a client's session attachments (which is
//! what the control connection grows into) and creating a session, which has no
//! rule yet for which host to create it on.

mod config;
mod directory;
mod enrollment;
mod link;
mod naming;
mod server;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::Mutex as TokioMutex;

use aj_app::cli::args::{Args, Command, DEFAULT_LISTEN_ADDRESS};
use aj_wire::{Hello, PROTOCOL_VERSION, SessionList, SessionSummary};
use anyhow::{Context, Result, bail};
use tokio::sync::watch;

use crate::gateway::config::{AddressError, GatewayConfig, HostAddress};
use crate::gateway::directory::{Directory, DirectoryError, Route};
use crate::gateway::enrollment::{
    EnrollmentError, EnrollmentFile, HostList, HostSource, HostSummary,
};
use crate::gateway::link::Link;
use crate::gateway::server::GatewayServer;
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

/// The timings a gateway runs on. Tuning, not policy: every value here changes
/// how quickly something is noticed, never what is true.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tuning {
    pub(crate) reconnect_delay: Duration,
    pub(crate) max_reconnect_delay: Duration,
    pub(crate) heartbeat: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            reconnect_delay: RECONNECT_DELAY,
            max_reconnect_delay: MAX_RECONNECT_DELAY,
            heartbeat: HEARTBEAT,
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
    #[error(transparent)]
    State(#[from] EnrollmentError),
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
    /// A remembered enrollment that the configuration now also names is dropped
    /// from the state file: the file is that host's record from here on, and
    /// keeping both would enroll it twice or resurrect it when the operator
    /// removes it from the configuration.
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
        for host in remembered {
            match directory.enroll(
                host.address.clone(),
                HostSource::Dynamic,
                Some(host.host_id.clone()),
            ) {
                Ok(()) => {}
                Err(err) => {
                    // The configuration got there first, which is the ordinary
                    // way this happens: the operator promoted a dynamically
                    // enrolled host into the file.
                    tracing::info!("dropping the remembered host {}: {err}", host.address);
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
            http: reqwest::Client::new(),
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
    pub(crate) fn sessions(&self) -> SessionList {
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
        {
            // Held across both, so the file cannot end up describing a set that
            // was never enrolled.
            let _writing = self.inner.writing.lock().await;
            self.inner.directory.enroll(
                address.clone(),
                HostSource::Dynamic,
                Some(hello.host_id.clone()),
            )?;
            if let Err(err) = self.remember() {
                // An enrollment the gateway cannot write down would come back as
                // a surprise absence after a restart, so it does not stand.
                let _ = self.inner.directory.withdraw(&hello.host_id);
                return Err(err);
            }
        }
        self.dial(address.clone());
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

    /// Remove the enrollment of `host_id` and stop following it.
    pub(crate) async fn withdraw(&self, host_id: &str) -> Result<(), GatewayError> {
        let address = {
            let _writing = self.inner.writing.lock().await;
            let address = self.inner.directory.withdraw(host_id)?;
            if let Err(err) = self.remember() {
                // A withdrawal the gateway cannot write down would come back as a
                // surprise after a restart, so it does not stand. The enrollment
                // goes back, its link was never stopped, and its rows return with
                // that host's next directory.
                let _ = self.inner.directory.enroll(
                    address,
                    HostSource::Dynamic,
                    Some(host_id.to_string()),
                );
                return Err(err);
            }
            address
        };
        self.undial(&address).await;
        Ok(())
    }

    /// Where a namespaced session id points, or why it does not.
    pub(crate) fn route(&self, id: &str) -> Result<Route, GatewayError> {
        Ok(self.inner.directory.route(id)?)
    }

    /// A receiver for the merged directory, for one attached client.
    pub(crate) fn subscribe(&self) -> watch::Receiver<Arc<Vec<SessionSummary>>> {
        self.inner.directory.subscribe()
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
    /// Nothing else has to be wound down: a gateway owns no sessions, no locks
    /// and no logs, which is the whole point of it holding none.
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
            self.inner.tuning,
        );
        if let Some(previous) = self.links().insert(address, link) {
            // Only reachable if an address were enrolled twice, which the
            // directory refuses. Stopping the old one anyway keeps a leaked task
            // from writing rows behind the new one's back.
            tokio::spawn(previous.stop());
        }
    }

    /// Stop the link to `address`, waiting for it to be gone.
    async fn undial(&self, address: &HostAddress) {
        let link = self.links().remove(address);
        if let Some(link) = link {
            link.stop().await;
        }
    }

    /// Write the dynamic enrollments down.
    fn remember(&self) -> Result<(), GatewayError> {
        Ok(self.inner.state.save(&self.inner.directory.dynamic())?)
    }

    fn links(&self) -> std::sync::MutexGuard<'_, HashMap<HostAddress, Link>> {
        self.inner.links.lock().expect("the link mutex is poisoned")
    }
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
    std::fs::create_dir_all(state_dir).map_err(|source| EnrollmentError::Read {
        path: state_dir.to_path_buf(),
        source,
    })?;
    let minted = format!("{:032x}", rand::random::<u128>());
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
    {
        Ok(mut file) => {
            std::io::Write::write_all(&mut file, format!("{minted}\n").as_bytes()).map_err(
                |source| EnrollmentError::Read {
                    path: path.clone(),
                    source,
                },
            )?;
            Ok(minted)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            read(&path)?.ok_or_else(|| {
                EnrollmentError::Write {
                    path: path.clone(),
                    reason: "the id file is empty: remove it to mint a fresh id".to_string(),
                }
                .into()
            })
        }
        Err(source) => Err(EnrollmentError::Read {
            path: path.clone(),
            source,
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
