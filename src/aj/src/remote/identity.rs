//! The connection identity gate (spec 6.11).
//!
//! An attached client can run arbitrary commands through the agent, so the
//! control port is remote code execution. The protocol itself stays
//! credential-free and this is the layer that decides who may speak it:
//! loopback-only by default, or a tailnet whois lookup that resolves the
//! peer's machine, user, tags and granted capabilities.
//!
//! The lookup sits behind [`WhoisResolver`] so the accept and reject paths
//! are testable without a tailnet. [`TailscaleWhois`] is the real one, and
//! it talks to the local tailscaled over its unix socket.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use aj_agent::BoxError;
use async_trait::async_trait;
use serde::Deserialize;
use serde::de::IgnoredAny;

/// The tailnet app capability that grants access to this control port.
///
/// Granted in the tailnet policy file and checked here, which is what lets a
/// tagged node in: a tagged node has no login to allowlist.
pub(crate) const AJ_CONTROL_CAPABILITY: &str = "github.com/aljoscha/aj/cap/control";

/// tailscaled's LocalAPI socket on Linux and macOS.
const TAILSCALED_SOCKET: &str = "/var/run/tailscale/tailscaled.sock";

/// The Host header tailscaled's LocalAPI accepts. It rejects requests naming
/// anything else, which is its own defense against DNS rebinding.
const LOCALAPI_HOST: &str = "local-tailscaled.sock";

/// How many peers' acceptance log lines are remembered, so a long-lived
/// server does not grow one entry per connection forever.
const LOGGED_PEERS: usize = 512;

/// How the server decides whether a peer may speak the protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdentityMode {
    /// Loopback peers only, and a refusal to serve a non-loopback address at
    /// all. The default.
    Local,
    /// Every connection's peer is resolved against the local tailscale
    /// daemon and checked against the allowlist and the app capability.
    Tailscale,
    /// Explicit opt-out, for a bind whose network is private by
    /// construction (the ember guest network).
    Open,
}

impl fmt::Display for IdentityMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Local => "local",
            Self::Tailscale => "tailscale",
            Self::Open => "open",
        };
        f.write_str(name)
    }
}

impl FromStr for IdentityMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "local" => Ok(Self::Local),
            "tailscale" => Ok(Self::Tailscale),
            "open" => Ok(Self::Open),
            other => Err(format!(
                "unknown auth mode {other:?}. Expected local, tailscale, or open"
            )),
        }
    }
}

/// Who a peer is, as the tailnet reports them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PeerIdentity {
    /// The tailnet login name, exactly as whois reports it (`alice@github`).
    /// Absent for a tagged node, which has no user behind it.
    pub(crate) login: Option<String>,
    /// The peer's machine name.
    pub(crate) node: String,
    pub(crate) tags: Vec<String>,
    /// The app capabilities the tailnet policy grants this peer.
    pub(crate) capabilities: BTreeSet<String>,
}

impl fmt::Display for PeerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.login {
            Some(login) => write!(f, "{login} on {}", self.node)?,
            None => write!(f, "{}", self.node)?,
        }
        if !self.tags.is_empty() {
            write!(f, " {:?}", self.tags)?;
        }
        Ok(())
    }
}

/// Why a peer was not let in.
///
/// Typed because the two layers that consume it answer differently: a bind
/// refusal happens at start-up and stops the process, while the other two are
/// per-request and answer 403. A failed lookup is deliberately
/// indistinguishable from a refusal to the peer, and only told apart in the
/// log.
#[derive(Debug, thiserror::Error)]
pub(crate) enum IdentityError {
    /// Serving this address in `local` mode would serve it unauthenticated.
    /// A start-up refusal, never a response.
    #[error(
        "refusing to serve {0} with --auth local: it is not a loopback address, \
         so every peer that can reach it would be accepted unauthenticated. \
         Bind loopback, or choose --auth tailscale"
    )]
    UnsafeBind(SocketAddr),
    /// The peer resolved and is not allowed in.
    #[error("{0}")]
    Forbidden(String),
    /// The peer could not be resolved, so it is refused: the gate fails
    /// closed.
    #[error("could not resolve the peer's tailnet identity: {0}")]
    Lookup(#[source] BoxError),
}

/// Resolves a connection's peer address to a tailnet identity.
///
/// Behind a trait so the gate's accept and reject paths are testable without
/// a tailnet. A resolver that cannot answer must return
/// [`IdentityError::Lookup`] rather than an empty identity: the gate fails
/// closed on the error, and an empty identity would pass whenever the
/// capability check happened to match.
#[async_trait]
pub(crate) trait WhoisResolver: Send + Sync {
    async fn resolve(&self, peer: SocketAddr) -> Result<PeerIdentity, IdentityError>;
}

/// The real resolver: tailscaled's LocalAPI over its unix socket, which is
/// what `tailscale whois` wraps.
pub(crate) struct TailscaleWhois {
    client: reqwest::Client,
    socket: PathBuf,
}

impl TailscaleWhois {
    /// A resolver against the local daemon's default socket path.
    pub(crate) fn new() -> Result<Self, IdentityError> {
        Self::at(Path::new(TAILSCALED_SOCKET))
    }

    /// A resolver against a specific socket path.
    pub(crate) fn at(socket: &Path) -> Result<Self, IdentityError> {
        let client = reqwest::Client::builder()
            .unix_socket(socket)
            .build()
            .map_err(|err| IdentityError::Lookup(Box::new(err)))?;
        Ok(Self {
            client,
            socket: socket.to_path_buf(),
        })
    }

    /// The socket this resolver queries, for diagnostics.
    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }
}

#[async_trait]
impl WhoisResolver for TailscaleWhois {
    async fn resolve(&self, peer: SocketAddr) -> Result<PeerIdentity, IdentityError> {
        let response = self
            .client
            .get(format!("http://{LOCALAPI_HOST}/localapi/v0/whois"))
            .query(&[("addr", peer.to_string())])
            .send()
            .await
            .map_err(|err| IdentityError::Lookup(Box::new(err)))?;
        let status = response.status();
        if !status.is_success() {
            // The daemon answers 404 for an address that is no tailnet peer,
            // which is a refusal rather than a malfunction. Both fail closed
            // here, so the distinction only shows in the log.
            return Err(IdentityError::Lookup(
                format!("tailscaled answered {status} for {peer}").into(),
            ));
        }
        let whois = response
            .bytes()
            .await
            .map_err(|err| IdentityError::Lookup(Box::new(err)))?;
        peer_identity_from_whois(&whois)
    }
}

/// Parse tailscaled's whois answer into the identity the gate checks.
///
/// Separated from the request so the shape of the daemon's answer is
/// testable without a tailnet. Unknown fields are ignored: the daemon's
/// model is much larger than this, and it grows.
pub(super) fn peer_identity_from_whois(json: &[u8]) -> Result<PeerIdentity, IdentityError> {
    let whois: WhoisResponse =
        serde_json::from_slice(json).map_err(|err| IdentityError::Lookup(Box::new(err)))?;
    Ok(whois.into_identity())
}

/// The fields of tailscaled's whois answer this gate needs. Everything else
/// in it, and anything a newer daemon adds, is ignored.
#[derive(Debug, Default, Deserialize)]
struct WhoisResponse {
    #[serde(rename = "Node")]
    node: Option<WhoisNode>,
    #[serde(rename = "UserProfile")]
    user_profile: Option<WhoisUserProfile>,
    /// The capability names granted to the peer. Only the keys matter, so
    /// the values are discarded rather than modelled.
    #[serde(rename = "CapMap")]
    cap_map: Option<BTreeMap<String, IgnoredAny>>,
}

#[derive(Debug, Default, Deserialize)]
struct WhoisNode {
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "Tags")]
    tags: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct WhoisUserProfile {
    #[serde(rename = "LoginName")]
    login_name: Option<String>,
}

impl WhoisResponse {
    fn into_identity(self) -> PeerIdentity {
        let node = self.node.unwrap_or_default();
        let tags = node.tags.unwrap_or_default();
        let login = self
            .user_profile
            .and_then(|profile| profile.login_name)
            .filter(|login| !login.is_empty())
            // NOTE: a tagged node still carries a synthetic user profile
            // (`tagged-devices`), which is no login anyone should be able to
            // allowlist. Tags are authoritative for such a node, so its
            // login is dropped here and only the capability can let it in.
            .filter(|_| tags.is_empty());
        PeerIdentity {
            login,
            node: node.name.unwrap_or_default(),
            tags,
            capabilities: self.cap_map.unwrap_or_default().into_keys().collect(),
        }
    }
}

/// Decides which peers may speak the protocol.
pub(crate) struct IdentityGate {
    mode: Mode,
    /// Peers already logged as accepted. A peer address carries the
    /// connection's ephemeral port, so "not seen before" is "a new
    /// connection" in practice, which is what makes the log one line per
    /// connection rather than one per request.
    logged: Mutex<LoggedPeers>,
}

enum Mode {
    Local,
    Open,
    Tailscale {
        allow: BTreeSet<String>,
        resolver: Arc<dyn WhoisResolver>,
    },
}

impl IdentityGate {
    /// Loopback peers only, and a start-up refusal for a non-loopback bind.
    pub(crate) fn local() -> Self {
        Self::with_mode(Mode::Local)
    }

    /// Every peer accepted. Only for a bind whose network is private by
    /// construction.
    pub(crate) fn open() -> Self {
        Self::with_mode(Mode::Open)
    }

    /// The whois gate: a peer is accepted when the lookup resolves and it
    /// carries either an allowlisted login or the app capability.
    pub(crate) fn tailscale(
        allow: impl IntoIterator<Item = String>,
        resolver: Arc<dyn WhoisResolver>,
    ) -> Self {
        Self::with_mode(Mode::Tailscale {
            allow: allow.into_iter().collect(),
            resolver,
        })
    }

    fn with_mode(mode: Mode) -> Self {
        Self {
            mode,
            logged: Mutex::new(LoggedPeers::default()),
        }
    }

    pub(crate) fn mode(&self) -> IdentityMode {
        match self.mode {
            Mode::Local => IdentityMode::Local,
            Mode::Open => IdentityMode::Open,
            Mode::Tailscale { .. } => IdentityMode::Tailscale,
        }
    }

    /// Whether this gate may serve `addr` at all.
    ///
    /// A `local` gate protects nothing on a non-loopback address: it would
    /// accept every peer that can reach it, unauthenticated. Refusing to
    /// start is the only safe answer, and it is deliberately not a 403,
    /// because the operator has to fix the configuration, not the client.
    pub(crate) fn validate_bind(&self, addr: SocketAddr) -> Result<(), IdentityError> {
        match self.mode {
            Mode::Local if !is_loopback(addr.ip()) => Err(IdentityError::UnsafeBind(addr)),
            _ => Ok(()),
        }
    }

    /// Authorize one connection's peer, returning the identity behind it
    /// when the mode resolves one.
    ///
    /// Every acceptance is logged once with the resolved identity, which is
    /// what gives an action an attributable origin. Request bodies are never
    /// logged: they carry prompts.
    pub(crate) async fn authorize(
        &self,
        peer: SocketAddr,
    ) -> Result<Option<PeerIdentity>, IdentityError> {
        match &self.mode {
            Mode::Open => {
                self.note_accepted(peer, None);
                Ok(None)
            }
            Mode::Local => {
                if !is_loopback(peer.ip()) {
                    return Err(IdentityError::Forbidden(format!(
                        "{peer} is not a loopback peer"
                    )));
                }
                self.note_accepted(peer, None);
                Ok(None)
            }
            Mode::Tailscale { allow, resolver } => {
                let identity = resolver.resolve(peer).await?;
                let allowed = identity
                    .login
                    .as_deref()
                    .is_some_and(|login| allow.contains(login))
                    || identity.capabilities.contains(AJ_CONTROL_CAPABILITY);
                if !allowed {
                    return Err(IdentityError::Forbidden(format!(
                        "{identity} is neither in the allowlist nor granted {AJ_CONTROL_CAPABILITY}"
                    )));
                }
                self.note_accepted(peer, Some(&identity));
                Ok(Some(identity))
            }
        }
    }

    fn note_accepted(&self, peer: SocketAddr, identity: Option<&PeerIdentity>) {
        if !self
            .logged
            .lock()
            .expect("identity gate log mutex poisoned")
            .note(peer)
        {
            return;
        }
        let mode = self.mode();
        match identity {
            Some(identity) => tracing::info!("accepted {peer} as {identity} (auth {mode})"),
            None => tracing::info!("accepted {peer} (auth {mode})"),
        }
    }
}

#[derive(Default)]
struct LoggedPeers {
    seen: HashSet<SocketAddr>,
    order: VecDeque<SocketAddr>,
}

impl LoggedPeers {
    /// Records `peer`, answering whether it is new and so worth a log line.
    /// The oldest entry is forgotten once the bound is reached, which can
    /// cost one duplicate line for a very long-lived connection.
    fn note(&mut self, peer: SocketAddr) -> bool {
        if !self.seen.insert(peer) {
            return false;
        }
        self.order.push_back(peer);
        if self.order.len() > LOGGED_PEERS
            && let Some(evicted) = self.order.pop_front()
        {
            self.seen.remove(&evicted);
        }
        true
    }
}

/// Whether `ip` names this machine, seeing through the IPv4-mapped IPv6 form
/// a dual-stack listener reports (`::ffff:127.0.0.1`), which
/// [`std::net::Ipv6Addr::is_loopback`] alone answers `false` for.
fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.is_loopback(),
            None => v6.is_loopback(),
        },
    }
}
