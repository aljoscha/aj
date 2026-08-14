//! The merged session directory (spec 7.1, 6.8).
//!
//! One entry per enrolled host, each holding the last directory that host sent
//! and whether its control connection is up. From those the gateway composes
//! one namespaced list, and the hosts it was composed from: the payload
//! `GET /v1/sessions` answers and the `list` frames its clients receive. There
//! is one composition, so a client reading and a client watching cannot
//! disagree.
//!
//! The gateway holds no session state of its own beyond these rows. A row is
//! whatever its host last said, kept as the JSON that host wrote, with three
//! fields the gateway owns: the namespaced id, the `host` it belongs to, and
//! `unreachable`. Everything else on it passes through unread, which is what
//! keeps a newer host's row a newer host's row (spec 6.10).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex as StdMutex};

use aj_app::host::AttachRequest;
use aj_wire::{DirectoryHost, HostList, HostSource, HostSummary, MergedDirectory, RawObject};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::gateway::config::HostAddress;
use crate::gateway::enrollment::{EnrolledHost, Recorded};
use crate::gateway::naming::{HostIdError, SessionAddress, addressable_session, validate_host_id};

/// The enrolled hosts and the one directory they merge into.
pub(crate) struct Directory {
    /// Keyed by address, because the address is what an operator enrolls and
    /// what a link dials. The host id is learned, and a configured host that
    /// has never answered does not have one yet.
    hosts: StdMutex<BTreeMap<HostAddress, Enrollment>>,
    /// The merged directory, republished whenever it actually changes.
    ///
    /// A watch rather than a queue per subscriber, because every frame this
    /// carries is a cumulative `list` snapshot: the newest supersedes and a
    /// slow reader wants only the latest (spec 6.4). Session frames are
    /// undroppable, so they travel a client's bounded queue instead
    /// ([`crate::gateway::outbound`]).
    merged: watch::Sender<Arc<MergedDirectory>>,
    /// The ids of the hosts whose control connection is up, republished
    /// whenever that set changes.
    ///
    /// The same facts [`Self::merged`] now names, on a channel of their own
    /// because they move at a different rate: the merged directory is
    /// republished for every row a busy host touches, and a splice watching it
    /// for reachability alone would wake once per client per refresh. A splice
    /// watches this because a host *returning* is what makes an upstream attach
    /// possible again, and `reset` is how a client is asked to make one
    /// (spec 7.1).
    reachable: watch::Sender<Arc<BTreeSet<String>>>,
}

/// One host as the gateway holds it.
struct Enrollment {
    source: HostSource,
    /// The id the host answers to, and the namespace its sessions appear under.
    ///
    /// `None` only for a host that has never answered: an id cannot be invented
    /// for a store nobody has spoken to. What a later answer under a different id
    /// does depends on how this enrollment anchors identity, see
    /// [`Directory::adopt`].
    host_id: Option<String>,
    /// The rows of this host's own last `list` frame, with its own ids.
    rows: Vec<Row>,
    connected: bool,
    /// Why the last connection attempt did not stick, for `GET /v1/hosts`.
    error: Option<String>,
    /// Cancelled when this identity stops being one this gateway serves, which
    /// is what ends the streams spliced onto it (spec 7.1): the enrollment is
    /// withdrawn, or a configured host answers under a different id and this
    /// token is replaced with the new identity's.
    ///
    /// Held here so that the set a withdrawal mutates and the signal it sends
    /// are the same thing under the same lock: a splice opened from a snapshot
    /// of this map holds a clone, and one opened after the withdrawal cannot
    /// resolve the host at all.
    serving: CancellationToken,
}

impl Enrollment {
    /// What this gateway knows the host by: its id, or the address it was
    /// enrolled at while it has never answered and so has no id.
    fn name(&self, address: &HostAddress) -> String {
        self.host_id.clone().unwrap_or_else(|| address.to_string())
    }
}

/// The fields of a directory row a gateway owns, spelled as
/// [`aj_wire::SessionSummary`] spells them (spec 6.8). Every other field on a
/// row belongs to the host that wrote it.
const ID_FIELD: &str = "id";
const HOST_FIELD: &str = "host";
const UNREACHABLE_FIELD: &str = "unreachable";

/// One row as its host wrote it, with the id this gateway routes and orders on
/// read out of it once.
///
/// The id comes off the row's own JSON rather than off a typed decode, so what
/// this gateway sorts and namespaces by is the very field it rewrites, and the
/// two cannot drift.
struct Row {
    id: String,
    raw: RawObject,
}

impl Row {
    /// One row of a host's directory, `None` for a row this gateway cannot read
    /// an id from, or could not route the id it reads.
    ///
    /// A row with no readable id has no namespace to appear under and nothing to
    /// route on, and re-emitting it under the host's own id would put an id no
    /// client here can address on the wire. An id [`addressable_session`]
    /// refuses is the same thing one step later: this gateway would publish it
    /// and then refuse the attach for it (spec 6.5), telling a client a session
    /// it can see is not there.
    ///
    /// A host's `list` frame is refused at decode long before this, so it takes
    /// a directory composed some other way to get here.
    fn read(raw: RawObject) -> Option<Self> {
        let id = match raw.get::<String>(ID_FIELD) {
            Ok(Some(id)) => id,
            outcome => {
                tracing::warn!("dropping a directory row this gateway cannot name: {outcome:?}");
                return None;
            }
        };
        if let Err(err) = addressable_session(&id) {
            tracing::warn!(
                "dropping the directory row {id:?}, which this gateway could not route: {err}"
            );
            return None;
        }
        Some(Self { id, raw })
    }

    /// This row as a client of this gateway sees it: the three fields a gateway
    /// owns rewritten, everything else the host's own JSON (spec 6.10).
    fn namespaced(&self, host_id: &str, connected: bool) -> RawObject {
        let mut raw = self.raw.clone();
        // A string and a bool always encode, so a failure here would be a
        // serializer bug rather than anything a host could send.
        own(&mut raw, host_id, &self.id, connected).expect("a string and a bool encode as JSON");
        raw
    }
}

/// Write the three fields a gateway owns onto a row it re-emits.
fn own(
    row: &mut RawObject,
    host_id: &str,
    session: &str,
    connected: bool,
) -> Result<(), serde_json::Error> {
    row.set(ID_FIELD, &SessionAddress::new(host_id, session).to_string())?;
    // Clients group by this rather than parsing the id, which is opaque
    // (spec 6.2, 6.8).
    row.set(HOST_FIELD, host_id)?;
    row.set(UNREACHABLE_FIELD, &!connected)
}

/// The client-visible teardown one identity's disappearance still owes, once its
/// rows are already out of the merged directory (spec 7.1).
///
/// Two edges produce one, and both are a withdrawal in the sense that matters to
/// a client: an enrollment removed ([`Directory::withdraw`]), and a configured
/// host answering under an id other than the one this gateway held for it
/// ([`Directory::adopt`]). Either way the ids that identity namespaced stop
/// resolving here, and the streams spliced onto it are still running.
#[derive(Debug)]
#[must_use = "the streams spliced onto an identity that is gone run until this ends them"]
pub(crate) struct Withdrawn {
    /// The identity's own token, which is what every splice onto it holds.
    serving: CancellationToken,
}

impl Withdrawn {
    /// End the streams spliced onto that identity, with the `reset` a withdrawal
    /// owes them.
    ///
    /// That `reset` asks the client to attach again, and the ids it names no
    /// longer resolve here, so each is refused with its own `error` frame and
    /// costs it that attachment and nothing else (spec 6.5). The directory,
    /// where those rows and that group are gone, says the same thing. See
    /// [`crate::gateway::splice`], which owns the frame.
    pub(crate) fn end_splices(self) {
        self.serving.cancel();
    }
}

/// What [`Directory::adopt`] settled.
#[derive(Debug)]
pub(crate) enum Adopted {
    /// The host named itself for the first time, so its id is new here and
    /// belongs in the gateway's record.
    Learned,
    /// It answered to the id this enrollment already had.
    Unchanged,
    /// A configured host answered under a different id, so the store this
    /// gateway was namespacing is gone and the identity that named it went with
    /// it (spec 7.1). Its rows have left, and its splices are what the caller
    /// still owes.
    Replaced(Withdrawn),
}

/// What settling a reported id against an enrollment amounts to, worked out
/// before anything is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Settling {
    Learned,
    Unchanged,
    Replaced,
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
    /// The enrollment's own token, cancelled when it is withdrawn.
    serving: CancellationToken,
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
        serving: enrollment.serving.clone(),
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
    /// Cancelled when this host's enrollment is withdrawn, which is what ends
    /// the upstream opened from this group (spec 7.1).
    pub(crate) serving: CancellationToken,
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

/// How one client's attach set divides among the enrolled hosts (spec 6.5).
///
/// A stream never fails wholesale over one bad session, so an id this gateway
/// cannot resolve is carried here rather than raised: the hosts it can reach
/// are served, and each of these is owed one session-scoped `error` frame.
pub(crate) struct AttachPlan {
    pub(crate) groups: Vec<AttachGroup>,
    pub(crate) refused: Vec<Unresolvable>,
}

/// A session a client named that resolves to no enrolled host (spec 6.5).
pub(crate) struct Unresolvable {
    /// The id as the client wrote it, which is the vocabulary its refusal has
    /// to name: it is what that client asked about, and it may be no id this
    /// gateway could ever have minted.
    pub(crate) session: String,
    /// The sentence the client renders, which says why this gateway resolved
    /// nothing (spec 6.6).
    pub(crate) message: String,
}

impl Directory {
    pub(crate) fn new() -> Self {
        let (merged, _) = watch::channel(Arc::new(MergedDirectory::default()));
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
                serving: CancellationToken::new(),
            },
        );
        self.publish(&hosts);
        Ok(())
    }

    /// Remove the enrollment of `host_id`, answering what remains to be torn
    /// down for it.
    ///
    /// The host's rows leave the merged directory here, in the same publish that
    /// removes it from the enrolled set, so nothing ever serves a directory that
    /// contradicts that set. What this does *not* do is end anything, see
    /// [`Withdrawn`].
    ///
    /// The refusals are [`Self::record_without`]'s, and a withdrawal reaches
    /// this only once that answer has been written down, so in practice they are
    /// unreachable here: the gateway holds one lock across both calls. Checked
    /// again rather than assumed, because the set is what this mutates.
    pub(crate) fn withdraw(
        &self,
        host_id: &str,
    ) -> Result<(HostAddress, Withdrawn), DirectoryError> {
        let mut hosts = self.lock();
        let address = withdrawable(&hosts, host_id)?;
        let enrollment = hosts.remove(&address).expect("the enrollment just found");
        self.publish(&hosts);
        Ok((
            address,
            Withdrawn {
                serving: enrollment.serving,
            },
        ))
    }

    /// Settle the id of the host at `address` against what it just reported.
    ///
    /// The two enrollment kinds anchor identity differently, so they answer a
    /// different id differently (spec 7.1). A dynamic enrollment names a host
    /// this gateway once shook hands with, so its recorded id is the record's
    /// referent and a different one is refused: that enrollment's host no longer
    /// exists, and the remedy is to withdraw it and enroll the address again. A
    /// configured enrollment names an address, so the operator's intent is
    /// whatever aj host answers there and its id is provisional: contact
    /// presenting a new one is handled as a withdrawal of the old identity
    /// followed by fresh contact, which is why the caller is handed a
    /// [`Withdrawn`] to finish.
    ///
    /// Answering what was settled is what tells the caller there is something to
    /// write down: an id is learned by speaking to the host, so this is the only
    /// place a configured host's ever gets settled.
    pub(crate) fn adopt(
        &self,
        address: &HostAddress,
        reported: &str,
    ) -> Result<Adopted, DirectoryError> {
        let mut hosts = self.lock();
        let settling = settling(&hosts, address, reported)?;
        if settling == Settling::Unchanged {
            return Ok(Adopted::Unchanged);
        }
        let enrollment = hosts
            .get_mut(address)
            .expect("the enrollment the settlement just read");
        let replaced = (settling == Settling::Replaced).then(|| {
            // The store this gateway was namespacing is gone with the identity
            // that named it. Its rows describe sessions that no longer exist, and
            // re-publishing them under the new id would name the new store's
            // sessions by the old store's rows.
            enrollment.rows.clear();
            // Nothing about the new identity is confirmed yet. Its link marks it
            // connected once its stream is open, which is moments from here.
            enrollment.connected = false;
            Withdrawn {
                serving: std::mem::replace(&mut enrollment.serving, CancellationToken::new()),
            }
        });
        enrollment.host_id = Some(reported.to_string());
        self.publish(&hosts);
        Ok(match replaced {
            Some(withdrawn) => Adopted::Replaced(withdrawn),
            None => Adopted::Learned,
        })
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
    ///
    /// The rows arrive as that host wrote them and are kept that way: only the
    /// id is read out, because it is what this gateway routes and orders on and
    /// the field it rewrites (spec 6.10).
    pub(crate) fn set_rows(&self, address: &HostAddress, rows: Vec<RawObject>) {
        let rows = rows.into_iter().filter_map(Row::read).collect();
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
    /// A stream never fails wholesale over one bad session (spec 6.5), so an id
    /// this gateway cannot resolve to an enrolled host is set aside for its own
    /// `error` frame rather than refusing the request: doing the latter would
    /// cost the client its healthy sessions on every other host. A host that is
    /// enrolled and not reachable is neither of those, see [`AttachGroup::dial`].
    pub(crate) fn group(&self, requests: &[AttachRequest]) -> AttachPlan {
        let hosts = self.lock();
        let mut groups: BTreeMap<String, AttachGroup> = BTreeMap::new();
        let mut refused = Vec::new();
        for request in requests {
            let owner = match owner(&hosts, &request.session) {
                Ok(owner) => owner,
                Err(err) => {
                    refused.push(Unresolvable {
                        session: request.session.clone(),
                        message: err.to_string(),
                    });
                    continue;
                }
            };
            let Owner {
                host_id,
                session,
                address,
                connected,
                serving,
            } = owner;
            let group = groups
                .entry(host_id.clone())
                .or_insert_with(|| AttachGroup {
                    host_id,
                    dial: connected.then(|| address.clone()),
                    attach: Vec::new(),
                    serving,
                });
            group.attach.push(AttachRequest {
                session,
                cursor: request.cursor.clone(),
            });
        }
        AttachPlan {
            groups: groups.into_values().collect(),
            refused,
        }
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

    /// The merged directory as it stands, which is the payload the sessions
    /// read answers with and the `list` frames carry.
    pub(crate) fn sessions(&self) -> Arc<MergedDirectory> {
        Arc::clone(&self.merged.borrow())
    }

    /// A receiver for the merged directory. The current value counts as seen,
    /// so a caller sends [`Self::sessions`] itself before waiting for changes.
    pub(crate) fn subscribe(&self) -> watch::Receiver<Arc<MergedDirectory>> {
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

    /// What belongs in the state file as the enrolled set stands (see
    /// [`Recorded`]).
    pub(crate) fn record(&self) -> Recorded {
        let hosts = self.lock();
        record(&hosts, None)
    }

    /// The same as it would stand once the host at `address` had settled
    /// `reported`, mutating nothing, or `None` when there is nothing to settle.
    ///
    /// This is what makes an adoption write-ahead, exactly as
    /// [`Self::record_without`] makes a withdrawal one: the record is written
    /// from this answer, and only then does the set change ([`Self::adopt`]), so
    /// a process that dies in between comes back holding the id its host
    /// reported rather than one this gateway has already stopped serving. It
    /// carries the refusals too, so an adoption that will not happen is never
    /// recorded, and the `None` keeps a link's every redial from rewriting the
    /// file with what it already says. The two answers agree because the gateway
    /// holds one lock across both calls.
    pub(crate) fn record_adopting(
        &self,
        address: &HostAddress,
        reported: &str,
    ) -> Result<Option<Recorded>, DirectoryError> {
        let hosts = self.lock();
        if settling(&hosts, address, reported)? == Settling::Unchanged {
            return Ok(None);
        }
        Ok(Some(record(&hosts, Some((address, reported)))))
    }

    /// The same as it would stand with `host_id` withdrawn, mutating nothing.
    ///
    /// This is what makes a withdrawal write-ahead: the record is written from
    /// this answer, and only then does the set change ([`Self::withdraw`]), so a
    /// write that fails has touched nothing and there is nothing to put back. It
    /// carries the refusals too, for the same reason: a withdrawal that will not
    /// happen must not be written down. The two answers agree because the gateway
    /// holds one lock across both calls.
    ///
    /// A configured host is refused: it would come straight back from the
    /// configuration file on the next start, so removing it here would be a
    /// promise the gateway cannot keep.
    pub(crate) fn record_without(&self, host_id: &str) -> Result<Recorded, DirectoryError> {
        let hosts = self.lock();
        let withdrawn = withdrawable(&hosts, host_id)?;
        let mut recorded = record(&hosts, None);
        recorded.hosts.retain(|host| host.address != withdrawn);
        Ok(recorded)
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

/// What the state file records about the enrolled set: the dynamic enrollments,
/// and the learned ids of the hosts the configuration file enrolls.
///
/// `adopting` names the one enrollment whose id is taken from that pair rather
/// than from the set, which is how an adoption is recorded before it lands (see
/// [`Directory::record_adopting`]).
///
/// A host that has never answered contributes nothing either way. There is no id
/// to write down for it, and its address is already in the configuration file
/// that named it.
fn record(
    hosts: &BTreeMap<HostAddress, Enrollment>,
    adopting: Option<(&HostAddress, &str)>,
) -> Recorded {
    let mut recorded = Recorded::default();
    for (address, enrollment) in hosts {
        let settled = match adopting {
            Some((adopting_at, reported)) if adopting_at == address => Some(reported.to_string()),
            _ => enrollment.host_id.clone(),
        };
        let Some(host_id) = settled else {
            continue;
        };
        let entry = EnrolledHost {
            address: address.clone(),
            host_id,
        };
        match enrollment.source {
            HostSource::Dynamic => recorded.hosts.push(entry),
            HostSource::Config => recorded.configured_ids.push(entry),
        }
    }
    recorded
}

/// What settling `reported` against the enrollment at `address` amounts to, or
/// why it cannot be settled at all.
///
/// In one place so that the answer an adoption is written down from and the set
/// it then mutates cannot disagree about what is being settled.
fn settling(
    hosts: &BTreeMap<HostAddress, Enrollment>,
    address: &HostAddress,
    reported: &str,
) -> Result<Settling, DirectoryError> {
    validate_host_id(reported).map_err(|source| DirectoryError::UnusableHostId {
        host_id: reported.to_string(),
        source,
    })?;
    let enrollment = hosts
        .get(address)
        .ok_or_else(|| DirectoryError::Withdrawn {
            address: address.clone(),
        })?;
    let known = match enrollment.host_id.as_deref() {
        Some(known) if known == reported => return Ok(Settling::Unchanged),
        // A dynamic enrollment is the record of the host it shook hands with, so
        // a different id at that address is a host this enrollment is not about.
        Some(known) if enrollment.source == HostSource::Dynamic => {
            return Err(DirectoryError::IdChanged {
                address: address.clone(),
                expected: known.to_string(),
                reported: reported.to_string(),
            });
        }
        known => known,
    };
    // One host id is one namespace, wherever the id comes from: two enrollments
    // answering to one id would give every session of that store two ids that
    // both route (see [`Directory::enroll`]).
    if let Some((taken, _)) = hosts.iter().find(|(enrolled_at, enrolled)| {
        *enrolled_at != address && enrolled.host_id.as_deref() == Some(reported)
    }) {
        return Err(DirectoryError::DuplicateHost {
            host_id: reported.to_string(),
            address: taken.clone(),
        });
    }
    Ok(match known {
        Some(_) => Settling::Replaced,
        None => Settling::Learned,
    })
}

/// The address of the enrollment `host_id` names, or why it cannot be withdrawn.
///
/// In one place so that the answer a withdrawal is written down from and the set
/// it then mutates cannot disagree about what is being withdrawn.
fn withdrawable(
    hosts: &BTreeMap<HostAddress, Enrollment>,
    host_id: &str,
) -> Result<HostAddress, DirectoryError> {
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
    Ok(address.clone())
}

/// Namespace every host's rows into one directory, and name the hosts it was
/// composed from.
///
/// Ordered latest first by the session's own id, which is how a host orders its
/// own rows (ids are minted as timestamps), with the host id breaking ties so
/// the order is total. Clients re-sort by activity anyway (spec 9.2), so this is
/// about being deterministic rather than about presentation.
///
/// Every enrolled host is named here, whether or not it has rows, because it may
/// have none for three different reasons: it is quiet, this gateway cannot reach
/// it and never stored what it last said, or it has never answered at all. A
/// client renders each as an empty group rather than as nothing, which is the
/// whole point of naming the hosts (spec 7.1). The last of them is named by its
/// address, because that is all this gateway knows it by: an id is learned by
/// asking, and a synthetic one would namespace sessions under a name that stops
/// being theirs the moment the host answers.
fn merge(hosts: &BTreeMap<HostAddress, Enrollment>) -> MergedDirectory {
    let mut rows: Vec<(&str, &Row, bool)> = Vec::new();
    let mut enrolled: Vec<DirectoryHost> = Vec::new();
    for (address, enrollment) in hosts {
        enrolled.push(DirectoryHost {
            id: enrollment.host_id.clone(),
            // A label only, and only where there is no id to label with. An
            // address is not something a client can address a session by.
            address: enrollment.host_id.is_none().then(|| address.to_string()),
            unreachable: !enrollment.connected,
        });
        // A host that has never answered has no id to namespace with, so its
        // rows have nowhere to appear even if it somehow sent some.
        let Some(host_id) = enrollment.host_id.as_deref() else {
            continue;
        };
        for row in &enrollment.rows {
            rows.push((host_id, row, enrollment.connected));
        }
    }
    rows.sort_by(|left, right| right.1.id.cmp(&left.1.id).then_with(|| left.0.cmp(right.0)));
    enrolled.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.address.cmp(&right.address))
    });
    MergedDirectory {
        sessions: rows
            .into_iter()
            .map(|(host_id, row, connected)| row.namespaced(host_id, connected))
            .collect(),
        hosts: enrolled,
    }
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
    /// A host answering to an id other than the one this gateway holds for it,
    /// on a dynamic enrollment, which is the record of the host it shook hands
    /// with. Only a link sees this, which records it and keeps redialing. A
    /// configured enrollment's id is provisional instead ([`Directory::adopt`]).
    #[error(
        "the host at {address} reports the id {reported:?} but this enrollment is the record \
         of {expected:?}: withdraw it and enroll {address} again if its store really changed"
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
    use aj_wire::SessionSummary;
    use chrono::{TimeZone, Utc};

    use super::*;

    fn address(raw: &str) -> HostAddress {
        HostAddress::parse(raw).expect("an address")
    }

    /// One row as a host writes it, plus a field this build has no type for, so
    /// that every assertion about the merge is also an assertion about what
    /// travels (spec 6.10).
    fn row(id: &str) -> RawObject {
        let mut raw = RawObject::encode(&SessionSummary {
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
            archived: false,
        })
        .expect("a row is an object");
        raw.set("preview", &format!("what {id} was doing"))
            .expect("a string encodes");
        raw
    }

    /// The merged rows decoded back into what a client reads, plus the text each
    /// one would arrive as.
    fn merged(directory: &Directory) -> Vec<(SessionSummary, String)> {
        directory
            .sessions()
            .sessions
            .iter()
            .map(|raw| {
                let json = serde_json::to_string(raw).expect("a row re-serializes");
                (
                    serde_json::from_str(&json).expect("a merged row is a row"),
                    json,
                )
            })
            .collect()
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

        let sessions = merged(&directory);

        assert_eq!(
            sessions
                .iter()
                .map(|(row, _)| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["left:s-2", "right:s-1"],
            "latest session first, across hosts",
        );
        assert_eq!(sessions[0].0.host.as_deref(), Some("left"));
        assert_eq!(sessions[1].0.host.as_deref(), Some("right"));
        assert!(sessions.iter().all(|(row, _)| !row.unreachable));
        assert_eq!(
            sessions[0].0.last_seq,
            Some(2),
            "everything else is the host's own answer, forwarded",
        );
        assert!(
            sessions[0].1.contains(r#""preview":"what s-2 was doing""#),
            "a field this gateway has no type for included: {}",
            sessions[0].1,
        );
    }

    /// A row this gateway cannot read an id from has no namespace to appear
    /// under and nothing to route on, so it does not travel. Its neighbours do:
    /// one unreadable row is not the host's whole directory.
    #[test]
    fn a_row_with_no_readable_id_is_dropped() {
        let directory = Directory::new();
        let address = connected(&directory, "127.0.0.1:1", "left", &["s-1"]);

        directory.set_rows(
            &address,
            vec![
                serde_json::from_str(r#"{"live":true}"#).expect("a row with no id"),
                serde_json::from_str(r#"{"id":42}"#).expect("a row whose id is no string"),
                row("s-2"),
            ],
        );

        assert_eq!(
            merged(&directory)
                .iter()
                .map(|(row, _)| row.id.clone())
                .collect::<Vec<_>>(),
            vec!["left:s-2".to_string()],
        );
    }

    /// A row this gateway would publish and then refuse to route is dropped
    /// where a row with no readable id is. `SessionAddress::parse` refuses an
    /// empty session half and a dot segment, so publishing one would mean
    /// answering the attach it invites with a refusal (spec 6.5) for a session
    /// the client had every reason to think was there.
    #[test]
    fn a_row_this_gateway_would_refuse_to_route_is_dropped() {
        let directory = Directory::new();
        let left = connected(&directory, "127.0.0.1:1", "left", &[]);
        connected(&directory, "127.0.0.1:2", "right", &["s-1"]);

        directory.set_rows(
            &left,
            ["", ".", "..", "s-2"].iter().map(|id| row(id)).collect(),
        );

        let published: Vec<String> = merged(&directory)
            .iter()
            .map(|(row, _)| row.id.clone())
            .collect();
        let attach: Vec<AttachRequest> = published.iter().map(|id| attaching(id, None)).collect();
        assert!(
            directory.group(&attach).refused.is_empty(),
            "this gateway published ids it will not resolve, which costs any \
             client that attaches them a refusal per session: {published:?}",
        );
        assert_eq!(
            published,
            vec!["left:s-2".to_string(), "right:s-1".to_string()],
            "and the rows those ids arrived beside still travel",
        );
    }

    #[test]
    fn a_disconnected_hosts_rows_stay_and_are_marked() {
        let directory = Directory::new();
        let left = connected(&directory, "127.0.0.1:1", "left", &["s-2"]);
        connected(&directory, "127.0.0.1:2", "right", &["s-1"]);

        directory.disconnected(&left, "connection refused".to_string());

        let sessions = merged(&directory);
        assert_eq!(sessions.len(), 2, "nothing is dropped: {sessions:?}");
        assert!(sessions[0].0.unreachable, "left's row: {:?}", sessions[0]);
        assert!(!sessions[1].0.unreachable, "right is unaffected");
        assert_eq!(
            directory.hosts().hosts[0].error.as_deref(),
            Some("connection refused"),
        );

        directory.connected(&left);
        assert!(
            merged(&directory).iter().all(|(row, _)| !row.unreachable),
            "and the mark clears when it answers again",
        );
        assert_eq!(directory.hosts().hosts[0].error, None);
    }

    /// A host that has never answered has no id, so it has no namespace and
    /// cannot contribute rows. It is named all the same, by the address it is
    /// enrolled at, which is what a client labels the empty group by (spec 7.1).
    #[test]
    fn a_host_with_no_id_yet_is_named_by_its_address() {
        let directory = Directory::new();
        let address = address("127.0.0.1:1");
        directory
            .enroll(address.clone(), HostSource::Config, None)
            .expect("enroll");
        directory.set_rows(&address, vec![row("s-1")]);

        assert_eq!(
            directory.sessions().hosts,
            vec![DirectoryHost {
                id: None,
                address: Some(address.to_string()),
                unreachable: true,
            }],
            "a client has a group to render, and nothing in the id position: an \
             id namespaces sessions, and a synthetic one would poison every id \
             this client holds the moment the real one arrived (spec 7.1)",
        );
        assert!(
            validate_host_id(&address.to_string()).is_err(),
            "and the label could never be taken for an id if it did reach that \
             position: {address}",
        );
        assert!(
            directory.sessions().sessions.is_empty(),
            "no namespace, so no rows either",
        );
        assert_eq!(directory.hosts().hosts.len(), 1);
        assert_eq!(directory.hosts().hosts[0].id, None);
        assert_eq!(
            directory.record(),
            Recorded::default(),
            "and there is nothing to write down about it: an address is already \
             in the configuration file that named it",
        );

        directory.adopt(&address, "learned").expect("adopt");
        assert_eq!(
            merged(&directory)[0].0.id,
            "learned:s-1",
            "its rows join the directory as soon as it names itself",
        );
        assert_eq!(
            directory.sessions().hosts,
            vec![DirectoryHost {
                id: Some("learned".to_string()),
                address: None,
                unreachable: true,
            }],
            "and its group is keyed by that id, with no address left to label by",
        );
    }

    /// A learned id is written down for every enrollment, not only for the ones
    /// the state file is the record of (spec 7.1).
    ///
    /// The two records are kept apart because they mean different things: a
    /// dynamic enrollment exists because the file says so, while a configured
    /// host's entry is identity only, so restoring it can never bring back a host
    /// the operator removed from the configuration.
    #[test]
    fn a_learned_id_is_recorded_whichever_way_the_host_was_enrolled() {
        let directory = Directory::new();
        connected(&directory, "127.0.0.1:1", "dynamic", &["s-1"]);
        let configured = address("127.0.0.1:2");
        directory
            .enroll(configured.clone(), HostSource::Config, None)
            .expect("enroll");
        let quiet = address("127.0.0.1:3");
        directory
            .enroll(quiet.clone(), HostSource::Config, None)
            .expect("enroll");

        assert_eq!(
            directory.record(),
            Recorded {
                hosts: vec![EnrolledHost {
                    address: address("127.0.0.1:1"),
                    host_id: "dynamic".to_string(),
                }],
                configured_ids: Vec::new(),
            },
            "a configured host that has never answered has no id to record",
        );

        directory.adopt(&configured, "learned").expect("adopt");

        assert_eq!(
            directory.record(),
            Recorded {
                hosts: vec![EnrolledHost {
                    address: address("127.0.0.1:1"),
                    host_id: "dynamic".to_string(),
                }],
                configured_ids: vec![EnrolledHost {
                    address: configured,
                    host_id: "learned".to_string(),
                }],
            },
            "and the one that answered is recorded by identity, without the file \
             becoming the record of it being enrolled at all",
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

    /// A dynamic enrollment names a host this gateway once shook hands with, so
    /// its recorded id is the record's referent and is fixed for the life of the
    /// enrollment (spec 7.1). A different id at that address is a different
    /// store, which this enrollment is not the record of, so it is refused and
    /// the refusal names the remedy that works here.
    ///
    /// The same first contact on a configured enrollment settles nothing for
    /// good, see [`a_configured_hosts_id_is_provisional`]: that one is enrolled
    /// by address, so the operator's intent is whatever host answers there.
    #[test]
    fn an_adopted_id_is_fixed_for_the_life_of_a_dynamic_enrollment() {
        let directory = Directory::new();
        let address = connected(&directory, "127.0.0.1:1", "left", &["s-1"]);

        assert!(
            matches!(directory.adopt(&address, "left"), Ok(Adopted::Unchanged)),
            "an id this enrollment already had is not news: a link reports one on \
             every redial, and each would otherwise rewrite the gateway's record",
        );
        let Err(err) = directory.adopt(&address, "other") else {
            panic!("a dynamic enrollment's id is the record's referent");
        };
        assert!(matches!(err, DirectoryError::IdChanged { .. }));
        let refusal = err.to_string();
        assert!(
            refusal.contains("withdraw") && refusal.contains("enroll"),
            "the refusal has to name the remedy that actually works for a dynamic \
             enrollment, which is withdrawing it and enrolling the address again: \
             {refusal}",
        );
        assert_eq!(merged(&directory)[0].0.id, "left:s-1");
        assert!(
            matches!(
                directory.record_adopting(&address, "other"),
                Err(DirectoryError::IdChanged { .. }),
            ),
            "and it is refused where an adoption is written down from too, so \
             nothing is recorded for one that will not happen",
        );

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
        assert!(
            matches!(directory.adopt(&fresh, "learned"), Ok(Adopted::Learned)),
            "and the first id a host reports is what there is to write down",
        );
    }

    /// A configured enrollment names an address, so the operator's intent is
    /// "whatever aj host answers here" and its id is provisional: contact
    /// presenting a different one is a rebuilt host, handled as a withdrawal of
    /// the old identity followed by fresh contact (spec 7.1).
    ///
    /// Invalidating the old namespaced ids is not the hazard it looks like. The
    /// sessions they named are gone with the store that held them, so the rows
    /// leave, the ids stop resolving, and the splices onto that identity are owed
    /// exactly what a withdrawal owes them.
    #[test]
    fn a_configured_hosts_id_is_provisional() {
        let directory = Directory::new();
        let address = enrolled_without_id(&directory);
        assert!(matches!(
            directory.adopt(&address, "before"),
            Ok(Adopted::Learned),
        ));
        directory.connected(&address);
        directory.set_rows(&address, vec![row("s-1")]);
        // The token a splice onto that host holds, taken the way a client stream
        // takes it.
        let watching = directory
            .group(&[attaching("before:s-1", None)])
            .groups
            .swap_remove(0)
            .serving;

        let Ok(Adopted::Replaced(withdrawn)) = directory.adopt(&address, "after") else {
            panic!("a configured host's first contact under a new id replaces the old one");
        };

        assert!(
            !watching.is_cancelled(),
            "replacing the id in the set does not end anything by itself",
        );
        withdrawn.end_splices();
        assert!(
            watching.is_cancelled(),
            "the splices onto the identity that is gone went on carrying frames \
             under a namespace no client of this gateway can address any more",
        );
        assert!(
            matches!(
                directory.route("before:s-1"),
                Err(DirectoryError::UnknownSession { .. }),
            ),
            "and an id namespaced under it still resolves, to a store that is gone",
        );
        assert!(
            merged(&directory).is_empty(),
            "the old store's rows describe sessions that no longer exist: {:?}",
            merged(&directory),
        );
        assert_eq!(
            directory.sessions().hosts,
            vec![DirectoryHost {
                id: Some("after".to_string()),
                address: None,
                unreachable: true,
            }],
            "the group is the new identity's, and nothing about that one is \
             confirmed until its own link says so",
        );
        assert_eq!(
            directory.record().configured_ids,
            vec![EnrolledHost {
                address: address.clone(),
                host_id: "after".to_string(),
            }],
            "and the id this gateway is the record of is the one that answered",
        );

        assert!(
            matches!(directory.adopt(&address, "after"), Ok(Adopted::Unchanged)),
            "the new identity is settled like any other: a redial reporting it \
             again is not news",
        );
        let opened = directory
            .group(&[attaching("after:s-1", None)])
            .groups
            .swap_remove(0)
            .serving;
        assert!(
            !opened.is_cancelled(),
            "a splice onto the host that answered here would end the moment it \
             opened, because the enrollment kept the token the old identity's \
             teardown cancelled",
        );
    }

    /// What an adoption would record is answered from a directory that has not
    /// moved, which is what lets the record be written before the change lands
    /// (spec 7.1).
    #[test]
    fn what_an_adoption_records_is_answered_before_it_happens() {
        let directory = Directory::new();
        let dynamic = connected(&directory, "127.0.0.1:1", "dynamic", &["s-1"]);
        let configured = enrolled_without_id(&directory);
        let mut merged_watch = directory.subscribe();

        assert_eq!(
            directory
                .record_adopting(&dynamic, "dynamic")
                .expect("an id this enrollment already has"),
            None,
            "an id that is not news is nothing to write down: a link reports one \
             on every redial",
        );
        assert_eq!(
            directory
                .record_adopting(&configured, "learned")
                .expect("a first contact")
                .expect("something to write down")
                .configured_ids,
            vec![EnrolledHost {
                address: configured.clone(),
                host_id: "learned".to_string(),
            }],
        );
        assert!(
            !merged_watch.has_changed().expect("the directory is alive"),
            "answering published an edge for an adoption that has not happened",
        );

        directory.adopt(&configured, "learned").expect("adopt");
        assert!(
            merged_watch.has_changed().expect("alive"),
            "the adoption itself is what publishes, and nothing below measures a \
             quiet answer unless this one was loud",
        );
        merged_watch.mark_unchanged();

        assert_eq!(
            directory
                .record_adopting(&configured, "rebuilt")
                .expect("a configured host's id is provisional")
                .expect("something to write down")
                .configured_ids,
            vec![EnrolledHost {
                address: configured.clone(),
                host_id: "rebuilt".to_string(),
            }],
            "the record a replacement writes is the new identity's, not the one \
             it is about to stop serving",
        );
        assert_eq!(
            directory.hosts().hosts[1].id.as_deref(),
            Some("learned"),
            "and the set has not moved: answering is not adopting",
        );

        // The refusals are the adoption's own, checked where it is written down
        // from so that one that will not happen is never recorded.
        assert!(matches!(
            directory.record_adopting(&configured, "dynamic"),
            Err(DirectoryError::DuplicateHost { .. }),
        ));
        assert!(matches!(
            directory.record_adopting(&configured, "with:colon"),
            Err(DirectoryError::UnusableHostId { .. }),
        ));
        assert!(matches!(
            directory.record_adopting(&address("127.0.0.1:9"), "stranger"),
            Err(DirectoryError::Withdrawn { .. }),
        ));
        assert!(
            !merged_watch.has_changed().expect("alive"),
            "answering published an edge for an adoption that has not happened",
        );
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
    /// when that host returns. An id that resolves to no host at all is set
    /// aside for its own refusal, which is the other half of the same rule.
    #[test]
    fn an_attach_set_groups_by_the_host_that_owns_each_session() {
        let directory = Directory::new();
        let left = connected(&directory, "127.0.0.1:1", "left", &["s-1", "s-2"]);
        connected(&directory, "127.0.0.1:2", "right", &["s-9"]);

        let plan = directory.group(&[
            attaching("left:s-1", Some("epoch-1:3")),
            attaching("right:s-9", None),
            attaching("left:s-2", None),
        ]);

        assert_eq!(
            rendered(&plan.groups),
            vec![
                "left@http://127.0.0.1:1: s-1@epoch-1:3, s-2".to_string(),
                "right@http://127.0.0.1:2: s-9".to_string(),
            ],
            "one group per host, in the client's own order within it",
        );
        assert!(plan.refused.is_empty(), "every id names an enrolled host");
        assert_eq!(
            plan.groups[0].namespaced(),
            vec!["left:s-1".to_string(), "left:s-2".to_string()],
            "and the group knows what a client of this gateway calls them",
        );

        directory.disconnected(&left, "gone".to_string());
        let plan = directory.group(&[attaching("left:s-1", None)]);
        assert_eq!(rendered(&plan.groups), vec!["left@-: s-1".to_string()]);
        assert!(
            plan.refused.is_empty(),
            "a host that is not there is not a refusal",
        );

        let unresolvable = ["s-1", "absent:s-1", ":s-1", "left:", "left:.."];
        let plan = directory.group(
            &unresolvable
                .iter()
                .map(|id| attaching(id, None))
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            plan.refused
                .iter()
                .map(|refused| refused.session.as_str())
                .collect::<Vec<_>>(),
            unresolvable,
            "each id that names no session here is owed its own refusal, by the \
             name the client gave it",
        );
        assert!(plan.groups.is_empty(), "and none of them opens an upstream",);
        assert!(
            plan.refused
                .iter()
                .all(|refused| refused.message.contains("names no session here")),
            "the refusal says why: {:?}",
            plan.refused
                .iter()
                .map(|refused| refused.message.as_str())
                .collect::<Vec<_>>(),
        );
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
            directory.record().hosts.len(),
            1,
            "a configured host's existence is the configuration file's record, \
             not this one's",
        );
        // Refused where a withdrawal is written down from, so a withdrawal that
        // will not happen is never recorded, and refused again where it mutates.
        assert!(matches!(
            directory.record_without("static"),
            Err(DirectoryError::StaticHost { .. }),
        ));
        assert!(matches!(
            directory.record_without("absent"),
            Err(DirectoryError::UnknownHost { .. }),
        ));
        assert!(matches!(
            directory.withdraw("static"),
            Err(DirectoryError::StaticHost { .. }),
        ));
        assert!(matches!(
            directory.withdraw("absent"),
            Err(DirectoryError::UnknownHost { .. }),
        ));

        // The token a splice onto that host holds, taken before the withdrawal
        // the way a client stream takes it.
        let watching = directory
            .group(&[attaching("dynamic:s-1", None)])
            .groups
            .swap_remove(0)
            .serving;
        let merged_watch = directory.subscribe();
        let reachable = directory.reachable();

        // The record a withdrawal writes before it does anything: the set as it
        // will stand, from a directory that has not moved.
        assert_eq!(
            directory
                .record_without("dynamic")
                .expect("a dynamic host")
                .hosts,
            Vec::new(),
            "what gets written down is the set without the host being withdrawn",
        );
        assert!(
            !reachable.has_changed().expect("the directory is alive")
                && !merged_watch.has_changed().expect("alive"),
            "answering that published an edge for a withdrawal that has not \
             happened, which a splice cannot be told to forget",
        );
        assert_eq!(
            merged(&directory)[0].0.id,
            "dynamic:s-1",
            "and the host is untouched until that write has landed, rows included",
        );
        assert!(!watching.is_cancelled());

        let (withdrawn_at, withdrawn) = directory.withdraw("dynamic").expect("withdraw");
        assert_eq!(withdrawn_at, address("127.0.0.1:1"));
        assert!(
            directory.sessions().sessions.is_empty(),
            "the rows leave with the enrollment",
        );
        assert_eq!(
            directory
                .sessions()
                .hosts
                .iter()
                .map(|host| host.id.clone())
                .collect::<Vec<_>>(),
            vec![Some("static".to_string())],
            "and so does its group, while the host that stayed keeps its own",
        );
        assert!(
            !watching.is_cancelled(),
            "removing an enrollment from the set does not end anything by itself",
        );

        withdrawn.end_splices();
        assert!(
            watching.is_cancelled(),
            "the token a withdrawal ends is the one the splice onto that host \
             took, or that stream runs on for a host that is gone",
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
        assert_eq!(watcher.borrow_and_update().sessions.len(), 2);
    }

    /// Reachability is a channel of its own because it moves at a different
    /// rate: the merged directory is republished for every row a busy host
    /// touches, and a splice watching that for reachability alone would wake
    /// once per client per refresh (spec 7.1).
    #[tokio::test]
    async fn a_row_change_does_not_republish_reachability() {
        let directory = Directory::new();
        let address = connected(&directory, "127.0.0.1:1", "left", &["s-1"]);
        let reachable = directory.reachable();
        let merged = directory.subscribe();

        directory.set_rows(&address, vec![row("s-1"), row("s-2")]);

        assert!(
            merged.has_changed().expect("the directory is alive"),
            "the row was news to the directory, and this measures nothing unless it was",
        );
        assert!(
            !reachable.has_changed().expect("alive"),
            "one host's row churn woke every splice waiting for a host to return",
        );

        directory.disconnected(&address, "gone".to_string());
        assert!(
            reachable.has_changed().expect("alive"),
            "and a host that went away is what this channel is for",
        );
    }
}
