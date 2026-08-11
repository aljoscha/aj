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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};

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
    /// slow reader wants only the latest (spec 6.4). Session frames, which are
    /// undroppable and need a bounded queue with eviction, are stage 3's.
    merged: watch::Sender<Arc<Vec<SessionSummary>>>,
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

/// Where a proxied request goes: which host, and what that host calls the
/// session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Route {
    pub(crate) address: HostAddress,
    pub(crate) session: String,
}

impl Directory {
    pub(crate) fn new() -> Self {
        let (merged, _) = watch::channel(Arc::new(Vec::new()));
        Self {
            hosts: StdMutex::new(BTreeMap::new()),
            merged,
        }
    }

    /// Enroll `address`, with `host_id` when it is already known.
    ///
    /// Two refusals keep the enrolled set free of duplicates (spec 7.1): one
    /// address is one enrollment, and one host id is one namespace. The second
    /// matters more than it looks: two enrollments of one store would give every
    /// session of it two ids that both route, and a client would see it twice.
    pub(crate) fn enroll(
        &self,
        address: HostAddress,
        source: HostSource,
        host_id: Option<String>,
    ) -> Result<(), DirectoryError> {
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
        let unknown = |reason: String| DirectoryError::UnknownSession {
            id: id.to_string(),
            reason,
        };
        let address = SessionAddress::parse(id).map_err(|err| unknown(err.to_string()))?;
        let hosts = self.lock();
        let (dial, enrollment) = hosts
            .iter()
            .find(|(_, enrolled)| enrolled.host_id.as_deref() == Some(address.host.as_str()))
            .ok_or_else(|| unknown(format!("no host {} is enrolled here", address.host)))?;
        if !enrollment.connected {
            return Err(DirectoryError::Unreachable {
                host_id: address.host.clone(),
            });
        }
        Ok(Route {
            address: dial.clone(),
            session: address.session,
        })
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
    /// and an identical snapshot carries no information (spec 6.8).
    fn publish(&self, hosts: &BTreeMap<HostAddress, Enrollment>) {
        let merged = merge(hosts);
        self.merged.send_if_modified(|current| {
            if **current == merged {
                return false;
            }
            *current = Arc::new(merged);
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
    #[error("host {host_id} is not reachable from this gateway")]
    Unreachable { host_id: String },
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
