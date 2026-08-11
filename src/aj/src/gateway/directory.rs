//! The merged session directory (spec 7.1, 6.8).
//!
//! One entry per enrolled host, each holding the last directory that host sent
//! and whether its control connection is up. From those the gateway composes
//! one namespaced list: the payload `GET /v1/sessions` answers and the `list`
//! frames its clients receive. There is one composition, so a client reading
//! and a client watching cannot disagree.
//!
//! The gateway holds no session state of its own beyond these rows. A row is
//! whatever its host last said, with three fields the gateway owns: the
//! namespaced id, the `host` it belongs to, and `unreachable`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex as StdMutex};

use aj_app::host::AttachRequest;
use aj_wire::{HostList, HostSource, HostSummary, SessionList, SessionSummary};
use tokio::sync::watch;

use crate::gateway::config::HostAddress;
use crate::gateway::enrollment::EnrolledHost;
use crate::gateway::naming::{HostIdError, SessionAddress, validate_host_id};

/// The enrolled hosts and the one directory they merge into.
pub(crate) struct Directory {
    /// Keyed by address, because the address is what an operator enrolls and
    /// what a link dials. The host id is learned, and a configured host that
    /// has never answered does not have one yet.
    hosts: StdMutex<BTreeMap<HostAddress, Enrollment>>,
    /// The merged rows, republished whenever they actually change.
    ///
    /// A watch rather than a queue per subscriber, because every frame this
    /// carries is a cumulative `list` snapshot: the newest supersedes and a
    /// slow reader wants only the latest (spec 6.4). Session frames are
    /// undroppable, so they travel a client's bounded queue instead
    /// ([`crate::gateway::outbound`]).
    merged: watch::Sender<Arc<Vec<SessionSummary>>>,
    /// The ids of the hosts whose control connection is up, republished
    /// whenever that set changes.
    ///
    /// Not derivable from [`Self::merged`]: a host contributes rows only once
    /// it has sent a directory, and a client can hold sessions of one that
    /// currently contributes none. A splice watches this because a host
    /// *returning* is what makes an upstream attach possible again, and
    /// `reset` is how a client is asked to make one (spec 7.1).
    reachable: watch::Sender<Arc<BTreeSet<String>>>,
}

/// One host as the gateway holds it.
struct Enrollment {
    source: HostSource,
    /// The id the host answers to, and the namespace its sessions appear under.
    ///
    /// `None` only for a configured host that has never answered: an id cannot
    /// be invented for a store nobody has spoken to. Once set it never changes
    /// (see [`Directory::adopt`]).
    host_id: Option<String>,
    /// The rows of this host's own last `list` frame, with its own ids.
    rows: Vec<SessionSummary>,
    connected: bool,
    /// Why the last connection attempt did not stick, for `GET /v1/hosts`.
    error: Option<String>,
}

impl Enrollment {
    /// What this gateway knows the host by: its id, or the address it was
    /// enrolled at while it has never answered and so has no id.
    fn name(&self, address: &HostAddress) -> String {
        self.host_id.clone().unwrap_or_else(|| address.to_string())
    }
}

/// Where a proxied request goes: which host, and what that host calls the
/// session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Route {
    pub(crate) address: HostAddress,
    /// The id the host answers to, which is the namespace this gateway's clients
    /// address its sessions under.
    pub(crate) host_id: String,
    pub(crate) session: String,
}

/// The enrollment a namespaced session id resolves to.
struct Owner<'a> {
    /// The id the host answers to, which is the namespace the session id
    /// carried.
    host_id: String,
    /// What that host calls the session.
    session: String,
    /// The address the host is enrolled at, which is what a link or an upstream
    /// dials.
    address: &'a HostAddress,
    /// Whether this gateway's control connection to the host is up.
    connected: bool,
}

/// Resolve a namespaced session id against the enrolled hosts.
///
/// In one place so that a proxied request and a spliced attach cannot disagree
/// about which ids name a session here. Reachability is reported rather than
/// judged: a command refuses on it ([`Directory::route`]) and a stream does not
/// ([`Directory::group`]).
fn owner<'a>(
    hosts: &'a BTreeMap<HostAddress, Enrollment>,
    id: &str,
) -> Result<Owner<'a>, DirectoryError> {
    let unknown = |reason: String| DirectoryError::UnknownSession {
        id: id.to_string(),
        reason,
    };
    let named = SessionAddress::parse(id).map_err(|err| unknown(err.to_string()))?;
    let (address, enrollment) = hosts
        .iter()
        .find(|(_, enrolled)| enrolled.host_id.as_deref() == Some(named.host.as_str()))
        .ok_or_else(|| unknown(format!("no host {} is enrolled here", named.host)))?;
    Ok(Owner {
        host_id: named.host,
        session: named.session,
        address,
        connected: enrollment.connected,
    })
}

/// Which host a create lands on: the host to forward it to, and the id its
/// sessions are namespaced under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostTarget {
    pub(crate) address: HostAddress,
    pub(crate) host_id: String,
}

/// The sessions one client attached on one host (spec 7.1).
///
/// One group is one upstream stream: the host's own ids with the client's own
/// cursors, ready to travel as they arrived.
pub(crate) struct AttachGroup {
    pub(crate) host_id: String,
    /// The address to open the upstream at, `None` when this gateway's control
    /// connection to the host is down.
    ///
    /// A host that is not there contributes no upstream rather than failing the
    /// client's whole stream, which would punish the sessions of every other
    /// host on it. Those sessions read `unreachable` in the list, which is what
    /// tells the client they carry nothing, and the host's return prompts the
    /// `reset` that makes it attach them again (spec 7.1).
    pub(crate) dial: Option<HostAddress>,
    /// The attach set as the owning host names it: de-namespaced ids, the
    /// client's cursors untouched.
    pub(crate) attach: Vec<AttachRequest>,
}

impl AttachGroup {
    /// The namespaced ids of this group's sessions, as a client of this gateway
    /// addresses them.
    pub(crate) fn namespaced(&self) -> Vec<String> {
        self.attach
            .iter()
            .map(|request| SessionAddress::new(&self.host_id, &request.session).to_string())
            .collect()
    }
}

impl Directory {
    pub(crate) fn new() -> Self {
        let (merged, _) = watch::channel(Arc::new(Vec::new()));
        let (reachable, _) = watch::channel(Arc::new(BTreeSet::new()));
        Self {
            hosts: StdMutex::new(BTreeMap::new()),
            merged,
            reachable,
        }
    }

    /// Enroll `address`, with `host_id` when it is already known.
    ///
    /// Two refusals keep the enrolled set free of duplicates (spec 7.1): one
    /// address is one enrollment, and one host id is one namespace. The second
    /// matters more than it looks: two enrollments of one store would give every
    /// session of it two ids that both route, and a client would see it twice.
    ///
    /// A third refuses a `host_id` outside the grammar, checked here where it
    /// arrives and not only where it is adopted ([`Self::adopt`]): an id this
    /// gateway cannot namespace with can never connect, so recording one would
    /// leave an enrollment that is broken for as long as it is kept, in the set
    /// and in the state file, reporting itself only as a host that never answers.
    pub(crate) fn enroll(
        &self,
        address: HostAddress,
        source: HostSource,
        host_id: Option<String>,
    ) -> Result<(), DirectoryError> {
        if let Some(host_id) = &host_id {
            validate_host_id(host_id).map_err(|source| DirectoryError::UnusableHostId {
                host_id: host_id.clone(),
                source,
            })?;
        }
        let mut hosts = self.lock();
        if hosts.contains_key(&address) {
            return Err(DirectoryError::AddressEnrolled { address });
        }
        if let Some(host_id) = &host_id
            && let Some((taken, _)) = hosts
                .iter()
                .find(|(_, enrolled)| enrolled.host_id.as_deref() == Some(host_id.as_str()))
        {
            return Err(DirectoryError::DuplicateHost {
                host_id: host_id.clone(),
                address: taken.clone(),
            });
        }
        hosts.insert(
            address,
            Enrollment {
                source,
                host_id,
                rows: Vec::new(),
                connected: false,
                error: None,
            },
        );
        self.publish(&hosts);
        Ok(())
    }

    /// Remove the enrollment of `host_id`, answering the address whose link is
    /// now to be stopped.
    ///
    /// A configured host is refused: it would come straight back from the
    /// configuration file on the next start, so removing it here would be a
    /// promise the gateway cannot keep.
    pub(crate) fn withdraw(&self, host_id: &str) -> Result<HostAddress, DirectoryError> {
        let mut hosts = self.lock();
        let (address, enrollment) = hosts
            .iter()
            .find(|(_, enrolled)| enrolled.host_id.as_deref() == Some(host_id))
            .ok_or_else(|| DirectoryError::UnknownHost {
                host_id: host_id.to_string(),
            })?;
        if enrollment.source == HostSource::Config {
            return Err(DirectoryError::StaticHost {
                address: address.clone(),
            });
        }
        let address = address.clone();
        hosts.remove(&address);
        self.publish(&hosts);
        Ok(address)
    }

    /// Settle the id of the host at `address` against what it just reported.
    ///
    /// The first answer wins for the life of the enrollment. A host that later
    /// reports a different id is refused rather than re-namespaced: every id a
    /// client holds for that host would silently stop resolving, and re-issuing
    /// them is what `reset` and a fresh enrollment are for. A host whose store
    /// really did change identity is re-enrolled by hand.
    pub(crate) fn adopt(
        &self,
        address: &HostAddress,
        reported: &str,
    ) -> Result<(), DirectoryError> {
        validate_host_id(reported).map_err(|source| DirectoryError::UnusableHostId {
            host_id: reported.to_string(),
            source,
        })?;
        let mut hosts = self.lock();
        let taken = hosts
            .iter()
            .find(|(enrolled_at, enrolled)| {
                *enrolled_at != address && enrolled.host_id.as_deref() == Some(reported)
            })
            .map(|(enrolled_at, _)| enrolled_at.clone());
        let enrollment = hosts
            .get_mut(address)
            .ok_or_else(|| DirectoryError::Withdrawn {
                address: address.clone(),
            })?;
        match enrollment.host_id.as_deref() {
            Some(known) if known == reported => return Ok(()),
            Some(known) => {
                return Err(DirectoryError::IdChanged {
                    address: address.clone(),
                    expected: known.to_string(),
                    reported: reported.to_string(),
                });
            }
            None => {}
        }
        if let Some(taken) = taken {
            return Err(DirectoryError::DuplicateHost {
                host_id: reported.to_string(),
                address: taken,
            });
        }
        enrollment.host_id = Some(reported.to_string());
        self.publish(&hosts);
        Ok(())
    }

    /// Note that the host at `address` is answering again.
    ///
    /// Its rows are left alone: they are the last thing it said, they stop being
    /// marked unreachable here, and its next `list` frame replaces them.
    pub(crate) fn connected(&self, address: &HostAddress) {
        let mut hosts = self.lock();
        let Some(enrollment) = hosts.get_mut(address) else {
            return;
        };
        enrollment.connected = true;
        enrollment.error = None;
        self.publish(&hosts);
    }

    /// Note that the host at `address` is not there, and why.
    ///
    /// Its rows stay in the directory and are marked `unreachable` (spec 6.8):
    /// a client that knows the session exists and cannot reach it is better
    /// served than one whose sidebar row vanished.
    pub(crate) fn disconnected(&self, address: &HostAddress, reason: String) {
        let mut hosts = self.lock();
        let Some(enrollment) = hosts.get_mut(address) else {
            return;
        };
        enrollment.connected = false;
        enrollment.error = Some(reason);
        self.publish(&hosts);
    }

    /// Replace what the host at `address` last said about its sessions.
    pub(crate) fn set_rows(&self, address: &HostAddress, rows: Vec<SessionSummary>) {
        let mut hosts = self.lock();
        let Some(enrollment) = hosts.get_mut(address) else {
            return;
        };
        enrollment.rows = rows;
        self.publish(&hosts);
    }

    /// Where a namespaced session id points.
    ///
    /// An id no enrollment can hold is an unknown session rather than a
    /// malformed request: ids are opaque to clients (spec 6.2), so a client
    /// cannot be expected to tell the difference and nothing it could fix is
    /// being reported. A host that is down is 503, which is the one status a
    /// gateway has that a host does not (spec 6.1).
    ///
    /// Down means "this gateway's control connection to it is down", the same
    /// thing the row's `unreachable` flag says, and a host whose port would in
    /// fact answer is refused all the same. That is deliberate: what a client is
    /// told about a session and what happens when it acts on one have to agree,
    /// and the window is bounded by the link's redial.
    pub(crate) fn route(&self, id: &str) -> Result<Route, DirectoryError> {
        let hosts = self.lock();
        let owner = owner(&hosts, id)?;
        if !owner.connected {
            return Err(DirectoryError::Unreachable {
                host: owner.host_id,
            });
        }
        Ok(Route {
            address: owner.address.clone(),
            host_id: owner.host_id,
            session: owner.session,
        })
    }

    /// Group a client's attach set by the host that owns each session
    /// (spec 7.1).
    ///
    /// The refusal is [`Self::route`]'s: an id this gateway cannot resolve to an
    /// enrolled host names no session here, and refusing the whole stream for it
    /// is what a host does with an attach it cannot serve (spec 6.5). A host
    /// that is enrolled and not reachable is *not* a refusal, see
    /// [`AttachGroup::dial`].
    pub(crate) fn group(
        &self,
        requests: &[AttachRequest],
    ) -> Result<Vec<AttachGroup>, DirectoryError> {
        let hosts = self.lock();
        let mut groups: BTreeMap<String, AttachGroup> = BTreeMap::new();
        for request in requests {
            let Owner {
                host_id,
                session,
                address,
                connected,
            } = owner(&hosts, &request.session)?;
            let group = groups
                .entry(host_id.clone())
                .or_insert_with(|| AttachGroup {
                    host_id,
                    dial: connected.then(|| address.clone()),
                    attach: Vec::new(),
                });
            group.attach.push(AttachRequest {
                session,
                cursor: request.cursor.clone(),
            });
        }
        Ok(groups.into_values().collect())
    }

    /// Which host a create is for (spec 6.6).
    ///
    /// `named` is the create body's `host` field, in the vocabulary the
    /// directory rows' `host` field uses. Naming none defaults to the sole
    /// enrolled host and to nothing else: with none enrolled there is nowhere
    /// to create, and with several the request is ambiguous, which is refused
    /// rather than guessed at, because a session created on the wrong host is
    /// in the wrong working directory and cannot be moved.
    ///
    /// A target whose control connection is down is unreachable, which is the
    /// same answer a proxied command to it gets and for the same reason
    /// ([`Self::route`]): what a client is told about a host and what happens
    /// when it acts on one have to agree.
    pub(crate) fn create_target(&self, named: Option<&str>) -> Result<HostTarget, DirectoryError> {
        let hosts = self.lock();
        let (address, enrollment) = match named {
            Some(named) => hosts
                .iter()
                .find(|(_, enrolled)| enrolled.host_id.as_deref() == Some(named))
                .ok_or_else(|| DirectoryError::UnknownHost {
                    host_id: named.to_string(),
                })?,
            None => {
                let mut enrollments = hosts.iter();
                let sole = enrollments.next().ok_or(DirectoryError::NoHostEnrolled)?;
                if enrollments.next().is_some() {
                    return Err(DirectoryError::AmbiguousHost {
                        hosts: hosts
                            .iter()
                            .map(|(address, enrollment)| enrollment.name(address))
                            .collect(),
                    });
                }
                sole
            }
        };
        // A host that has never answered has no id, and so is not connected
        // either: a link adopts an id before it reports a connection. Both read
        // as unreachable, which is what a gateway can honestly say about a host
        // it has not spoken to.
        match (&enrollment.host_id, enrollment.connected) {
            (Some(host_id), true) => Ok(HostTarget {
                address: address.clone(),
                host_id: host_id.clone(),
            }),
            _ => Err(DirectoryError::Unreachable {
                host: enrollment.name(address),
            }),
        }
    }

    /// The merged directory as it stands.
    pub(crate) fn sessions(&self) -> SessionList {
        SessionList {
            sessions: (*self.merged.borrow()).as_ref().clone(),
        }
    }

    /// A receiver for the merged directory. The current value counts as seen,
    /// so a caller sends [`Self::sessions`] itself before waiting for changes.
    pub(crate) fn subscribe(&self) -> watch::Receiver<Arc<Vec<SessionSummary>>> {
        self.merged.subscribe()
    }

    /// A receiver for the ids of the hosts this gateway can reach.
    ///
    /// The current value counts as seen, so a caller that wants to act on an
    /// edge takes the state it compares against from elsewhere. A splice takes
    /// it from the groups it opened, which is what keeps a host that came up
    /// between the two an edge rather than a state this stream has to guess at.
    pub(crate) fn reachable(&self) -> watch::Receiver<Arc<BTreeSet<String>>> {
        self.reachable.subscribe()
    }

    /// The enrolled hosts, for `GET /v1/hosts`.
    pub(crate) fn hosts(&self) -> HostList {
        let hosts = self.lock();
        HostList {
            hosts: hosts
                .iter()
                .map(|(address, enrollment)| HostSummary {
                    id: enrollment.host_id.clone(),
                    address: address.to_string(),
                    source: enrollment.source,
                    connected: enrollment.connected,
                    sessions: enrollment.rows.len(),
                    error: enrollment.error.clone(),
                })
                .collect(),
        }
    }

    /// The enrollments that belong in the state file: the dynamic ones, which
    /// are the only ones the gateway is the record of.
    pub(crate) fn dynamic(&self) -> Vec<EnrolledHost> {
        let hosts = self.lock();
        hosts
            .iter()
            .filter(|(_, enrollment)| enrollment.source == HostSource::Dynamic)
            .filter_map(|(address, enrollment)| {
                Some(EnrolledHost {
                    address: address.clone(),
                    host_id: enrollment.host_id.clone()?,
                })
            })
            .collect()
    }

    /// Every enrolled address, for the links to be spawned or stopped over.
    pub(crate) fn addresses(&self) -> Vec<HostAddress> {
        self.lock().keys().cloned().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<HostAddress, Enrollment>> {
        self.hosts.lock().expect("the directory mutex is poisoned")
    }

    /// Recompose the merged rows and publish them if they changed.
    ///
    /// Called with the map held, so what is published is what the map says: two
    /// mutations cannot interleave into a snapshot neither of them produced. An
    /// unchanged payload is not published at all, because `list` is cumulative
    /// and an identical snapshot carries no information (spec 6.8), and because
    /// every attached client watches both of these: a host's ordinary directory
    /// refresh must not wake one of them per client per frame.
    fn publish(&self, hosts: &BTreeMap<HostAddress, Enrollment>) {
        let merged = merge(hosts);
        self.merged.send_if_modified(|current| {
            if **current == merged {
                return false;
            }
            *current = Arc::new(merged);
            true
        });
        let reachable: BTreeSet<String> = hosts
            .values()
            .filter(|enrollment| enrollment.connected)
            .filter_map(|enrollment| enrollment.host_id.clone())
            .collect();
        self.reachable.send_if_modified(|current| {
            if **current == reachable {
                return false;
            }
            *current = Arc::new(reachable);
            true
        });
    }
}

/// Namespace every host's rows into one directory.
///
/// Ordered latest first by the session's own id, which is how a host orders its
/// own rows (ids are minted as timestamps), with the host id breaking ties so
/// the order is total. Clients re-sort by activity anyway (spec 9.2), so this is
/// about being deterministic rather than about presentation.
fn merge(hosts: &BTreeMap<HostAddress, Enrollment>) -> Vec<SessionSummary> {
    let mut rows: Vec<(&str, &SessionSummary, bool)> = Vec::new();
    for enrollment in hosts.values() {
        // A host that has never answered has no id to namespace with, and
        // therefore no rows either.
        let Some(host_id) = enrollment.host_id.as_deref() else {
            continue;
        };
        for row in &enrollment.rows {
            rows.push((host_id, row, enrollment.connected));
        }
    }
    rows.sort_by(|left, right| right.1.id.cmp(&left.1.id).then_with(|| left.0.cmp(right.0)));
    rows.into_iter()
        .map(|(host_id, row, connected)| SessionSummary {
            id: SessionAddress::new(host_id, &row.id).to_string(),
            // A gateway fills this in as it merges, and clients group by it
            // rather than parsing the id (spec 6.8).
            host: Some(host_id.to_string()),
            unreachable: !connected,
            ..row.clone()
        })
        .collect()
}

/// Why a directory operation did not do what was asked.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DirectoryError {
    #[error("{address} is enrolled already")]
    AddressEnrolled { address: HostAddress },
    #[error(
        "host {host_id} is enrolled already, at {address}: two enrollments of one host \
         would put its sessions in one namespace twice"
    )]
    DuplicateHost {
        host_id: String,
        address: HostAddress,
    },
    #[error("no host {host_id} is enrolled here")]
    UnknownHost { host_id: String },
    #[error(
        "{address} comes from the gateway configuration, which would enroll it again on the \
         next start: remove it there instead"
    )]
    StaticHost { address: HostAddress },
    #[error("{id} names no session here: {reason}")]
    UnknownSession { id: String, reason: String },
    /// The gateway's control connection to the host is down, so it will not
    /// serve a request for it.
    #[error("host {host} is not reachable from this gateway")]
    Unreachable {
        /// What this gateway knows the host by: its id, or its address when it
        /// has never answered and so has no id.
        host: String,
    },
    /// A create named no host and there is more than one to choose from. Never
    /// guessed at (spec 6.6).
    #[error(
        "this gateway has {} hosts enrolled, so a create has to name one of them: {}",
        hosts.len(),
        hosts.join(", ")
    )]
    AmbiguousHost { hosts: Vec<String> },
    /// A create arrived at a gateway that has no host to create on.
    #[error("no host is enrolled on this gateway, so there is nowhere to create a session")]
    NoHostEnrolled,
    #[error("a host reports the id {host_id:?}, which cannot be used here: {source}")]
    UnusableHostId {
        host_id: String,
        #[source]
        source: HostIdError,
    },
    /// A host answering to an id other than the one it was enrolled under. Only
    /// a link sees this, which records it and keeps redialing.
    #[error(
        "the host at {address} reports the id {reported:?} but is enrolled as {expected:?}: \
         re-enroll it if its store really changed"
    )]
    IdChanged {
        address: HostAddress,
        expected: String,
        reported: String,
    },
    /// The enrollment went away while its link was connecting. Only a link sees
    /// this, and it is on its way out anyway.
    #[error("{address} is no longer enrolled")]
    Withdrawn { address: HostAddress },
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    fn address(raw: &str) -> HostAddress {
        HostAddress::parse(raw).expect("an address")
    }

    fn row(id: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            live: true,
            working: false,
            queued: aj_wire::QueueCounts::default(),
            tasks: 0,
            last_seq: Some(2),
            last_activity: Utc
                .timestamp_opt(1_800_000_000, 0)
                .single()
                .expect("a timestamp"),
            tag: None,
            host: None,
            unreachable: false,
        }
    }

    /// A host with `id` at `raw`, connected, holding `rows`.
    fn connected(directory: &Directory, raw: &str, id: &str, rows: &[&str]) -> HostAddress {
        let address = address(raw);
        directory
            .enroll(address.clone(), HostSource::Dynamic, Some(id.to_string()))
            .expect("enroll");
        directory.connected(&address);
        directory.set_rows(&address, rows.iter().map(|id| row(id)).collect());
        address
    }

    #[test]
    fn rows_are_namespaced_and_named_by_host() {
        let directory = Directory::new();
        connected(&directory, "127.0.0.1:1", "left", &["s-2"]);
        connected(&directory, "127.0.0.1:2", "right", &["s-1"]);

        let sessions = directory.sessions().sessions;

        assert_eq!(
            sessions
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["left:s-2", "right:s-1"],
            "latest session first, across hosts",
        );
        assert_eq!(sessions[0].host.as_deref(), Some("left"));
        assert_eq!(sessions[1].host.as_deref(), Some("right"));
        assert!(sessions.iter().all(|row| !row.unreachable));
        assert_eq!(
            sessions[0].last_seq,
            Some(2),
            "everything else is the host's own answer, forwarded",
        );
    }

    #[test]
    fn a_disconnected_hosts_rows_stay_and_are_marked() {
        let directory = Directory::new();
        let left = connected(&directory, "127.0.0.1:1", "left", &["s-2"]);
        connected(&directory, "127.0.0.1:2", "right", &["s-1"]);

        directory.disconnected(&left, "connection refused".to_string());

        let sessions = directory.sessions().sessions;
        assert_eq!(sessions.len(), 2, "nothing is dropped: {sessions:?}");
        assert!(sessions[0].unreachable, "left's row: {:?}", sessions[0]);
        assert!(!sessions[1].unreachable, "right is unaffected");
        assert_eq!(
            directory.hosts().hosts[0].error.as_deref(),
            Some("connection refused"),
        );

        directory.connected(&left);
        assert!(
            directory
                .sessions()
                .sessions
                .iter()
                .all(|row| !row.unreachable),
            "and the mark clears when it answers again",
        );
        assert_eq!(directory.hosts().hosts[0].error, None);
    }

    /// A host that has never answered has no id, so it has no namespace and
    /// cannot contribute rows. It is still an enrollment.
    #[test]
    fn a_host_with_no_id_yet_contributes_nothing() {
        let directory = Directory::new();
        let address = address("127.0.0.1:1");
        directory
            .enroll(address.clone(), HostSource::Config, None)
            .expect("enroll");
        directory.set_rows(&address, vec![row("s-1")]);

        assert!(directory.sessions().sessions.is_empty());
        assert_eq!(directory.hosts().hosts.len(), 1);
        assert_eq!(directory.hosts().hosts[0].id, None);
        assert!(
            directory.dynamic().is_empty(),
            "and there is nothing to write down about it",
        );

        directory.adopt(&address, "learned").expect("adopt");
        assert_eq!(
            directory.sessions().sessions[0].id,
            "learned:s-1",
            "its rows join the directory as soon as it names itself",
        );
    }

    #[test]
    fn one_address_and_one_host_id_are_each_enrolled_once() {
        let directory = Directory::new();
        connected(&directory, "127.0.0.1:1", "left", &[]);

        assert!(matches!(
            directory.enroll(address("http://127.0.0.1:1/"), HostSource::Dynamic, None),
            Err(DirectoryError::AddressEnrolled { .. }),
        ));
        assert!(matches!(
            directory.enroll(
                address("127.0.0.1:9"),
                HostSource::Dynamic,
                Some("left".to_string()),
            ),
            Err(DirectoryError::DuplicateHost { .. }),
        ));
        // The same collision the other way round: a second address that turns
        // out to answer to an id already enrolled.
        let second = address("127.0.0.1:8");
        directory
            .enroll(second.clone(), HostSource::Dynamic, None)
            .expect("an address of its own");
        assert!(matches!(
            directory.adopt(&second, "left"),
            Err(DirectoryError::DuplicateHost { .. }),
        ));
        assert_eq!(directory.hosts().hosts.len(), 2);
    }

    #[test]
    fn an_adopted_id_is_fixed_for_the_life_of_the_enrollment() {
        let directory = Directory::new();
        let address = connected(&directory, "127.0.0.1:1", "left", &["s-1"]);

        directory
            .adopt(&address, "left")
            .expect("the same id again");
        assert!(matches!(
            directory.adopt(&address, "other"),
            Err(DirectoryError::IdChanged { .. }),
        ));
        assert_eq!(directory.sessions().sessions[0].id, "left:s-1");

        // An id the gateway could not namespace with is refused before it is
        // ever recorded.
        let fresh = enrolled_without_id(&directory);
        assert!(matches!(
            directory.adopt(&fresh, "with:colon"),
            Err(DirectoryError::UnusableHostId { .. }),
        ));
        assert!(matches!(
            directory.adopt(&fresh, ""),
            Err(DirectoryError::UnusableHostId { .. }),
        ));
    }

    fn enrolled_without_id(directory: &Directory) -> HostAddress {
        let fresh = address("127.0.0.1:7");
        directory
            .enroll(fresh.clone(), HostSource::Config, None)
            .expect("enroll");
        fresh
    }

    #[test]
    fn a_route_names_the_owning_host_and_the_hosts_own_id() {
        let directory = Directory::new();
        let left = connected(&directory, "127.0.0.1:1", "left", &["s-1"]);
        connected(&directory, "127.0.0.1:2", "right", &["s-1"]);

        assert_eq!(
            directory.route("left:s-1").expect("a route"),
            Route {
                address: left.clone(),
                host_id: "left".to_string(),
                session: "s-1".to_string(),
            },
            "the de-namespaced id is what the owning host calls it",
        );
        // A session the other host does not have still routes to it: whether it
        // holds one is the host's answer to give, not the gateway's.
        assert_eq!(
            directory.route("right:absent").expect("a route").session,
            "absent",
        );
        for id in ["s-1", "absent:s-1", ":s-1", "left:"] {
            assert!(
                matches!(
                    directory.route(id),
                    Err(DirectoryError::UnknownSession { .. }),
                ),
                "{id:?} should not route",
            );
        }

        directory.disconnected(&left, "gone".to_string());
        assert!(matches!(
            directory.route("left:s-1"),
            Err(DirectoryError::Unreachable { .. }),
        ));
    }

    /// A client's attach set groups by the host that owns each session, in that
    /// host's own vocabulary and with the client's own cursors (spec 7.1).
    ///
    /// A host that is not reachable still gets a group, with nothing to dial: its
    /// sessions contribute no upstream rather than failing the client's whole
    /// stream, and the group is what tells this gateway whose `reset` to emit
    /// when that host returns.
    #[test]
    fn an_attach_set_groups_by_the_host_that_owns_each_session() {
        let directory = Directory::new();
        let left = connected(&directory, "127.0.0.1:1", "left", &["s-1", "s-2"]);
        connected(&directory, "127.0.0.1:2", "right", &["s-9"]);

        let groups = directory
            .group(&[
                attaching("left:s-1", Some("epoch-1:3")),
                attaching("right:s-9", None),
                attaching("left:s-2", None),
            ])
            .expect("every id names an enrolled host");

        assert_eq!(
            rendered(&groups),
            vec![
                "left@http://127.0.0.1:1: s-1@epoch-1:3, s-2".to_string(),
                "right@http://127.0.0.1:2: s-9".to_string(),
            ],
            "one group per host, in the client's own order within it",
        );
        assert_eq!(
            groups[0].namespaced(),
            vec!["left:s-1".to_string(), "left:s-2".to_string()],
            "and the group knows what a client of this gateway calls them",
        );

        directory.disconnected(&left, "gone".to_string());
        let groups = directory
            .group(&[attaching("left:s-1", None)])
            .expect("a host that is not there is not a refusal");
        assert_eq!(rendered(&groups), vec!["left@-: s-1".to_string()]);

        for id in ["s-1", "absent:s-1", ":s-1", "left:", "left:.."] {
            assert!(
                matches!(
                    directory.group(&[attaching(id, None)]),
                    Err(DirectoryError::UnknownSession { .. }),
                ),
                "{id:?} names no session here, so the stream is refused",
            );
        }
    }

    fn attaching(session: &str, cursor: Option<&str>) -> AttachRequest {
        AttachRequest {
            session: session.to_string(),
            cursor: cursor.map(|cursor| cursor.parse().expect("a cursor")),
        }
    }

    /// Each group as `<host>@<address or -> : <attach set>`, which is everything
    /// one upstream is opened from.
    fn rendered(groups: &[AttachGroup]) -> Vec<String> {
        groups
            .iter()
            .map(|group| {
                let attach: Vec<String> = group
                    .attach
                    .iter()
                    .map(|request| match &request.cursor {
                        Some(cursor) => format!("{}@{cursor}", request.session),
                        None => request.session.clone(),
                    })
                    .collect();
                let dial = match &group.dial {
                    Some(address) => address.to_string(),
                    None => "-".to_string(),
                };
                format!("{}@{dial}: {}", group.host_id, attach.join(", "))
            })
            .collect()
    }

    /// Which host a create lands on (spec 6.6): the one it names, the only one
    /// enrolled when it names none, and nothing guessed at in between.
    #[test]
    fn a_create_target_is_the_host_it_names_or_the_only_one_enrolled() {
        let directory = Directory::new();
        assert!(matches!(
            directory.create_target(None),
            Err(DirectoryError::NoHostEnrolled),
        ));

        let left = connected(&directory, "127.0.0.1:1", "left", &["s-1"]);
        let target = HostTarget {
            address: left.clone(),
            host_id: "left".to_string(),
        };
        assert_eq!(
            directory.create_target(None).expect("the only host"),
            target,
            "one enrolled host needs no naming",
        );
        assert_eq!(
            directory.create_target(Some("left")).expect("named"),
            target
        );
        assert!(
            matches!(
                directory.create_target(Some("absent")),
                Err(DirectoryError::UnknownHost { .. }),
            ),
            "a name no enrollment answers to is not a host to fall back from",
        );

        connected(&directory, "127.0.0.1:2", "right", &[]);
        let Err(DirectoryError::AmbiguousHost { hosts }) = directory.create_target(None) else {
            panic!("two enrolled hosts and no name is ambiguous");
        };
        assert_eq!(
            hosts,
            vec!["left".to_string(), "right".to_string()],
            "and the refusal names the hosts to choose between",
        );
        assert_eq!(
            directory
                .create_target(Some("right"))
                .expect("named")
                .host_id,
            "right",
        );

        directory.disconnected(&left, "gone".to_string());
        assert!(matches!(
            directory.create_target(Some("left")),
            Err(DirectoryError::Unreachable { .. }),
        ));
    }

    /// A configured host that has never answered is enrolled, so it counts
    /// towards a create having to name one, and it can never be the target
    /// itself: the session would have no namespace to appear under.
    #[test]
    fn a_host_with_no_id_yet_is_not_a_create_target() {
        let directory = Directory::new();
        let fresh = enrolled_without_id(&directory);

        let Err(DirectoryError::Unreachable { host }) = directory.create_target(None) else {
            panic!("a host this gateway has never spoken to cannot take a create");
        };
        assert_eq!(
            host,
            fresh.to_string(),
            "named by its address, which is all this gateway knows it by",
        );
    }

    #[test]
    fn only_a_dynamic_enrollment_is_withdrawn_and_written_down() {
        let directory = Directory::new();
        connected(&directory, "127.0.0.1:1", "dynamic", &["s-1"]);
        let configured = address("127.0.0.1:2");
        directory
            .enroll(
                configured.clone(),
                HostSource::Config,
                Some("static".to_string()),
            )
            .expect("enroll");

        assert_eq!(
            directory.dynamic().len(),
            1,
            "a configured host is the file's record, not the gateway's",
        );
        assert!(matches!(
            directory.withdraw("static"),
            Err(DirectoryError::StaticHost { .. }),
        ));
        assert!(matches!(
            directory.withdraw("absent"),
            Err(DirectoryError::UnknownHost { .. }),
        ));
        assert_eq!(
            directory.withdraw("dynamic").expect("withdraw"),
            address("127.0.0.1:1"),
        );
        assert!(
            directory.sessions().sessions.is_empty(),
            "and its rows go with it",
        );
    }

    /// An unchanged directory publishes nothing, so a subscriber is not woken
    /// for a snapshot it already has (spec 6.8).
    #[tokio::test]
    async fn an_unchanged_snapshot_is_not_republished() {
        let directory = Directory::new();
        let address = connected(&directory, "127.0.0.1:1", "left", &["s-1"]);
        let mut watcher = directory.subscribe();

        directory.set_rows(&address, vec![row("s-1")]);
        directory.connected(&address);
        assert!(
            !watcher.has_changed().expect("the directory is alive"),
            "the same rows and the same liveness is the same snapshot",
        );

        directory.set_rows(&address, vec![row("s-1"), row("s-2")]);
        assert!(watcher.has_changed().expect("alive"), "a new row is news");
        assert_eq!(watcher.borrow_and_update().len(), 2);
    }
}
