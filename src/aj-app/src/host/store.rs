//! The on-disk part of the host's session directory.
//!
//! A `list` frame is produced on a coalescing tick whose frequent trigger is
//! session events, so producing one may not touch the filesystem. It does
//! not: a refresh reads [`ColdSessions::rows`], which is memory. The rows
//! are brought up to date at enumeration points, which are rare and externally
//! paced (host startup, an explicit session listing, a stream attach), and by
//! the host recording its own structural changes.
//!
//! The host is the single writer of its working directory's store, so this
//! is not a staleness the design has to chase. A
//! concurrent writer's sessions cannot be served by this host anyway, and its
//! activity becomes visible at the next enumeration point. That cuts both ways:
//! a row whose log another process deleted is offered until then, and an attach
//! to it is refused, since membership is answered off the store rather than off
//! these rows.
//!
//! An enumeration does not open session logs. The only per-session file it
//! opens is a tag sidecar, only for cold sessions that have one. Labels are
//! read afresh at each enumeration point. The archived sidecars cost
//! one more listing of the same directory and no read at all: the file's
//! existence is the whole answer. A row itself is built from the `stat` the
//! enumeration already did, which is what keeps host startup off the store's
//! bytes: deriving a cold session's `last_seq` would cost a read of every log
//! in the directory, and the row does not carry one.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use aj_session::{
    ConversationError, ConversationPersistence, LockMetadata, SessionLock, SessionMetadata,
    SidecarMetadata,
};

use crate::host::live::ReleasedRow;
use chrono::{DateTime, Utc};

/// What a directory refresh needs from the session store.
///
/// Behind a trait because what this module exists for is the reads it does
/// *not* perform, which the values it returns cannot show. The tests drive it
/// with a store that counts them.
pub(crate) trait SessionStore {
    /// Every session log in the store, with its fingerprint. Opens no file.
    fn list_sessions(&self) -> Result<Vec<SessionMetadata>, ConversationError>;

    /// The fingerprint of one session's log, `Ok(None)` when the store holds
    /// no log under that id. One `stat`, no directory read.
    fn session_metadata(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionMetadata>, ConversationError>;

    /// Every tag sidecar in the store, with its fingerprint. One directory
    /// read, and none at all for a store that has no tagged session.
    fn enumerate_tags(&self) -> Result<Vec<SidecarMetadata>, ConversationError>;

    /// Every archived sidecar in the store, with its fingerprint. The same
    /// one directory read, for the axis whose answer is the file's existence.
    fn enumerate_archived(&self) -> Result<Vec<SidecarMetadata>, ConversationError>;

    /// Every session lock in the store, with whether it carries a holder
    /// record. One directory read plus a `stat` each, and no lock is asked
    /// about.
    fn enumerate_locks(&self) -> Result<Vec<LockMetadata>, ConversationError>;

    /// Whether a writer holds this session's lock right now. One non-blocking
    /// shared probe, which writes nothing.
    fn probe_lock(&self, session_id: &str) -> Result<bool, ConversationError>;

    /// The tag in one session's sidecar, `Ok(None)` when it has none or its
    /// sidecar says nothing usable. Opens the file and reads it.
    fn read_tag(&self, session_id: &str) -> Result<Option<String>, ConversationError>;
}

impl SessionStore for ConversationPersistence {
    fn list_sessions(&self) -> Result<Vec<SessionMetadata>, ConversationError> {
        ConversationPersistence::list_sessions(self)
    }

    fn session_metadata(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionMetadata>, ConversationError> {
        ConversationPersistence::session_metadata(self, session_id)
    }

    fn enumerate_tags(&self) -> Result<Vec<SidecarMetadata>, ConversationError> {
        ConversationPersistence::enumerate_tags(self)
    }

    fn enumerate_archived(&self) -> Result<Vec<SidecarMetadata>, ConversationError> {
        ConversationPersistence::enumerate_archived(self)
    }

    fn enumerate_locks(&self) -> Result<Vec<LockMetadata>, ConversationError> {
        SessionLock::enumerate_locks(self)
    }

    fn probe_lock(&self, session_id: &str) -> Result<bool, ConversationError> {
        SessionLock::is_held(self, session_id)
    }

    fn read_tag(&self, session_id: &str) -> Result<Option<String>, ConversationError> {
        ConversationPersistence::read_tag(self, session_id)
    }
}

/// One session the store holds that the host is not holding live.
///
/// No durable position: a cold row carries an activity stamp instead, and
/// deriving the position would cost a read of the log.
#[derive(Clone)]
pub(crate) struct ColdSession {
    pub(crate) id: String,
    pub(crate) last_activity: DateTime<Utc>,
    /// The session's label, `None` when it has no sidecar or none this host
    /// could read.
    pub(crate) tag: Option<String>,
    /// Whether the user has put the session away, which is the existence of
    /// its archived sidecar.
    pub(crate) archived: bool,
    /// Whether a writer that is not this host holds the session's lock.
    pub(crate) locked: bool,
}

/// The store's sessions as the host last saw them.
pub(crate) struct ColdSessions<S> {
    store: S,
    cache: StdMutex<Cache>,
    directory_reads: AtomicU64,
    sidecar_directory_reads: AtomicU64,
    membership_lookups: AtomicU64,
    lock_directory_reads: AtomicU64,
    lock_probes: AtomicU64,
}

/// A log file's identity for caching: a file whose modification time and size
/// have not moved cannot have changed shape.
///
/// Not a content hash, so a rewrite that preserves both is invisible to it.
/// Only a hand-edited log does that.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    modified: DateTime<Utc>,
    size: u64,
}

impl Fingerprint {
    fn of(modified: DateTime<Utc>, size: u64) -> Self {
        Self { modified, size }
    }
}

/// A cold row's activity stamp, plus the file state it describes.
///
/// The fingerprint is what lets the host's own knowledge outrank a `stat`. A
/// release records what its driver last saw the session do, against the file
/// it left behind, and an enumeration that finds that same file keeps it. The
/// modification time answers a different question, when the bytes landed, and
/// the release flush can land buffered entries a whole idle grace after the
/// work that wrote them (see [`ReleasedRow`]). Once the file moves under a
/// writer this host is not, the `stat` is all there is.
///
/// NOTE(aljoscha): that last rule is what a scan whose directory read has gone
/// stale falls foul of. A session that was cold when the scan started, then
/// went live, was appended to and released while it ran, has a row at a newer
/// file than the scan holds, and the scan overwrites it with its own older
/// `stat`. [`ColdSessions::evict`] refuses the same move for the same reason
/// and can, because it has the ids it read the directory after taking. Doing
/// it here would need a generation per row, for a window that takes a scan
/// outlasting the idle grace to open.
struct Row {
    at: Fingerprint,
    last_activity: DateTime<Utc>,
}

/// The maps are keyed by session id. [`ColdSessions::enumerate`] drops the
/// entries of sessions that have left the store, so on a host that ever
/// enumerates (every host does, at startup) none of them outgrows it. Entries
/// that [`ColdSessions::contains`] adds in between are not evicted until the
/// next enumeration.
///
/// Sidecars can outlive their logs. Labels also retain cleared publications
/// while the session exists, so overlapping scans cannot resurrect a label
/// after its sidecar has been removed.
#[derive(Default)]
struct Cache {
    /// The answer a refresh serves. What an enumeration point last found, plus
    /// what the host has recorded about its own sessions since.
    rows: HashMap<String, Row>,
    /// Labels remembered for memory-only publication. Each update has its own
    /// identity so a scan cannot overwrite or evict a newer publication, even
    /// when its text is unchanged. A release records clears as `None` for the
    /// same reason. A cleared publication stays while its session exists.
    tags: HashMap<String, Arc<Option<String>>>,
    /// One entry per session this host knows the archived bit of: those whose
    /// sidecar a listing found, and those a release recorded. Absence is the
    /// unarchived answer, which is what makes an unarchived store cost
    /// nothing.
    ///
    /// The bit is held rather than implied by the entry's presence, because a
    /// listing only ever reports the sidecars that exist. An entry saying
    /// `false` is what a release leaves behind to say the session was
    /// unarchived under its own lock, and it is what makes that release
    /// tellable from an id the cache never held (see
    /// [`ColdSessions::record_archived`]).
    ///
    /// The listing's own report that the sidecar exists is the whole answer.
    archived: HashMap<String, bool>,
    /// The sessions a rival writer holds, as the host last established. Its
    /// members are the rows that read `locked`.
    ///
    /// A set rather than a map of bits, because membership is the bit and the
    /// unheld answer is the overwhelmingly common one: a store whose sessions
    /// nobody else holds keeps this empty rather than an entry per lock file
    /// it has ever minted.
    ///
    /// Never contains a session this host holds live. A probe of one would
    /// read held, since `flock` belongs to the open file description, so the
    /// sweep skips them and a won acquire clears the entry a refusal left.
    locked: HashSet<String>,
    /// Identity of the current lock publication. A scan may publish only if
    /// no acquire or probe updated the cache while it read the filesystem.
    /// One identity for the whole sweep is sufficient for this advisory axis.
    lock_publication: Arc<()>,
}

impl<S: SessionStore> ColdSessions<S> {
    pub(crate) fn new(store: S) -> Self {
        Self {
            store,
            cache: StdMutex::new(Cache::default()),
            directory_reads: AtomicU64::new(0),
            sidecar_directory_reads: AtomicU64::new(0),
            membership_lookups: AtomicU64::new(0),
            lock_directory_reads: AtomicU64::new(0),
            lock_probes: AtomicU64::new(0),
        }
    }

    /// The cold rows as they stand, in no particular order.
    ///
    /// Touches no filesystem, which is the whole point: this is what a
    /// refresh serves.
    pub(crate) fn rows(&self) -> Vec<ColdSession> {
        let cache = self.cache();
        cache
            .rows
            .iter()
            .map(|(id, row)| ColdSession {
                id: id.clone(),
                last_activity: row.last_activity,
                tag: cache.tags.get(id).and_then(|tag| tag.as_ref().clone()),
                archived: cache.archived.get(id).copied().unwrap_or(false),
                locked: cache.locked.contains(id),
            })
            .collect()
    }

    /// The activity stamp this host holds for `id`, if it holds a row for it.
    ///
    /// Touches no filesystem.
    pub(crate) fn stamp(&self, id: &str) -> Option<DateTime<Utc>> {
        self.cache().rows.get(id).map(|row| row.last_activity)
    }

    /// The label this host holds for `id`, if it holds one.
    ///
    /// Touches no filesystem. What a materialization falls back to when it
    /// cannot read the sidecar itself: a read that failed says nothing about
    /// the label (see [`Self::tag`]), and what the last enumeration or release
    /// recorded is the best answer left.
    pub(crate) fn label(&self, id: &str) -> Option<String> {
        self.cache()
            .tags
            .get(id)
            .and_then(|tag| tag.as_ref().clone())
    }

    /// Whether this host last knew `id` to be archived.
    ///
    /// Touches no filesystem. What a materialization falls back to when it
    /// cannot read the sidecar itself, on the reasoning [`Self::label`] gives.
    pub(crate) fn archived(&self, id: &str) -> bool {
        self.cache().archived.get(id).copied().unwrap_or(false)
    }

    /// Re-read the store and bring the rows up to date. The enumeration point,
    /// and the only path here that reads the directory.
    ///
    /// `live` names the sessions the host holds. Their logs are enumerated like
    /// any other, but nothing is derived from them: the host answers a live
    /// session off its own status, which is both cheaper and more current than
    /// anything the file could say mid-append. Their rows are left as they
    /// stand rather than dropped, so a session released while this runs keeps
    /// the row its release recorded instead of falling out of the directory
    /// until the next enumeration point.
    pub(crate) fn enumerate(&self, live: impl Fn(&str) -> bool) -> Result<(), ConversationError> {
        // What this scan is entitled to evict, taken before the directory read
        // so that everything in it predates this scan's view of the store.
        //
        // Rows and labels get their own set. A row can only arrive mid-scan
        // under an id the cache never held, so the id alone tells this scan's
        // rows from a newer one's, while a label arrives on a session the
        // cache usually already holds a row for. The labels are therefore
        // taken with their identities, which is what makes one published while
        // the scan ran recognisable under an id the scan did see (see
        // [`Self::clear_missing_tags`]). Archived bits use value comparisons (see
        // [`Self::record_archived`]).
        let (known, labelled, filed, locks_before) = {
            let cache = self.cache();
            let known: HashSet<String> = cache.rows.keys().cloned().collect();
            (
                known,
                cache.tags.clone(),
                cache.archived.clone(),
                Arc::clone(&cache.lock_publication),
            )
        };
        let enumerated = self.enumerate_store()?;
        for metadata in &enumerated {
            if live(&metadata.session_id) {
                continue;
            }
            let at = fingerprint(metadata);
            let mut cache = self.cache();
            let row = cache.rows.get(&metadata.session_id);
            if row.is_none_or(|row| row.at != at) {
                cache.rows.insert(
                    metadata.session_id.clone(),
                    Row {
                        at,
                        last_activity: metadata.modified_at,
                    },
                );
            }
        }
        // The second directory read, over `meta/`. A store with no tagged
        // session has no such directory, so this costs one failed open and
        // reads nothing.
        //
        // A sidecar directory we cannot read costs the labels their refresh
        // and nothing else: a label is display metadata, and one that cannot
        // be re-read must not take a session's row down with it. The cached
        // labels stand until a scan gets a look at the files again, which is
        // also why nothing is evicted on this path.
        match self.enumerate_sidecars() {
            Ok(sidecars) => {
                for sidecar in &sidecars {
                    // A live session's label is the host's own, held in memory
                    // and handed to the cold cache by its release, so reading
                    // the file would only offer a staler answer.
                    if live(&sidecar.session_id) {
                        continue;
                    }
                    self.tag(&sidecar.session_id, labelled.get(&sidecar.session_id));
                }
                self.clear_missing_tags(&sidecars, &enumerated, &labelled);
            }
            Err(err) => tracing::warn!("could not read the store's tag sidecars: {err}"),
        }
        // The third, over the same directory for the second sidecar axis. It
        // costs one more `readdir` at an enumeration point and no per-file
        // read at all, since the sidecar's existence is the whole answer.
        match self.enumerate_archived_sidecars() {
            Ok(sidecars) => self.record_archived(&sidecars, &filed),
            Err(err) => tracing::warn!("could not read the store's archived sidecars: {err}"),
        }
        // The fourth, over `locks/`, for the one axis whose fact belongs to
        // another writer. A stat per lock file, and a probe only of
        // the ones a stat shows a holder record on.
        //
        // A directory this host cannot read costs the axis its refresh and
        // nothing else, on the same reasoning as the tag sidecars: the bit is a
        // hint, and one that cannot be re-established must not take a row down
        // with it.
        match self.enumerate_locks() {
            Ok(locks) => self.record_locked(&self.probe(&locks, &live), &locks_before),
            Err(err) => tracing::warn!("could not read the store's session locks: {err}"),
        }
        self.evict(&enumerated, &known);
        Ok(())
    }

    /// Ask which of `locks` a rival holds, probing as few of them as possible.
    fn probe(&self, locks: &[LockMetadata], live: &impl Fn(&str) -> bool) -> Vec<(String, bool)> {
        let mut verdicts = Vec::new();
        for lock in locks {
            // A session this host holds is never locked on its own rows, and
            // asking would say held anyway: `flock` belongs to the open file
            // description, so this host's own lock refuses this host's probe.
            if live(&lock.session_id) {
                continue;
            }
            if !lock.has_holder_record {
                // Nobody has taken this lock since the last release of it, so
                // it reads free unprobed and a settled store is swept without
                // a single probe. The one misread this permits is a holder that
                // failed to write its record, which reads free until an attempt
                // is refused and sets the bit, the answer that was always the
                // authority.
                verdicts.push((lock.session_id.clone(), false));
                continue;
            }
            match self.probe_lock(&lock.session_id) {
                Ok(held) => verdicts.push((lock.session_id.clone(), held)),
                // No verdict rather than a guess: the entry keeps whatever it
                // held, and the next attempt or sweep asks again.
                Err(err) => {
                    tracing::warn!("could not probe the lock of {}: {err}", lock.session_id)
                }
            }
        }
        verdicts
    }

    /// Record what a sweep established about the locks it probed.
    ///
    /// A concurrent publication outranks this sweep, even if the bit returned
    /// to its starting value. Skipping the sweep leaves an advisory snapshot
    /// until the next enumeration or probe, not permission to take a lock.
    fn record_locked(&self, verdicts: &[(String, bool)], locks_before: &Arc<()>) {
        let mut cache = self.cache();
        if !Arc::ptr_eq(&cache.lock_publication, locks_before) {
            return;
        }
        for (session_id, held) in verdicts {
            if *held {
                cache.locked.insert(session_id.clone());
            } else {
                cache.locked.remove(session_id);
            }
        }
        cache.lock_publication = Arc::new(());
    }

    /// Publish an acquire's authoritative answer, including repeated bits.
    pub(crate) fn note_locked(&self, session_id: &str, locked: bool) {
        let mut cache = self.cache();
        if locked {
            cache.locked.insert(session_id.to_string());
        } else {
            cache.locked.remove(session_id);
        }
        cache.lock_publication = Arc::new(());
    }

    /// Record a probe's falling edge.
    pub(crate) fn note_unlocked(&self, session_id: &str) -> bool {
        let mut cache = self.cache();
        let changed = cache.locked.remove(session_id);
        if changed {
            cache.lock_publication = Arc::new(());
        }
        changed
    }

    /// The sessions this host currently publishes as locked.
    ///
    /// Touches no filesystem. What the probe tick asks about, and empty in the
    /// normal state, which is what makes a tick over a settled host free.
    pub(crate) fn locked(&self) -> Vec<String> {
        self.cache().locked.iter().cloned().collect()
    }

    /// How many times this has read the store's directory.
    ///
    /// The refresh contract is about the filesystem work a refresh does *not*
    /// do, which its answers cannot show, so this is the seam the tests assert
    /// on. Only [`Self::enumerate`] reads the directory: a membership question
    /// is answered off a single `stat` (see [`Self::contains`]).
    ///
    /// The store's own directory. The sidecar one an enumeration also reads
    /// has its own counter, [`Self::sidecar_directory_reads`].
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn directory_reads(&self) -> u64 {
        self.directory_reads.load(Ordering::Relaxed)
    }

    /// How many times this has read the store's `meta/` directory.
    ///
    /// The same contract as [`Self::directory_reads`], for the sidecar
    /// directory an enumeration also reads, once per axis. It needs its own
    /// counter because a readdir and a `stat` transfer no bytes. A refresh
    /// that listed the sidecars would be invisible to a byte-read budget.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn sidecar_directory_reads(&self) -> u64 {
        self.sidecar_directory_reads.load(Ordering::Relaxed)
    }

    /// How many membership questions reached the store.
    ///
    /// The other half of the same kind of contract: an id the grammar rejects
    /// is turned away *before* any store lookup, and a refusal
    /// leaves no other trace to assert on, since the answer is the same
    /// either way.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn membership_lookups(&self) -> u64 {
        self.membership_lookups.load(Ordering::Relaxed)
    }

    /// How many times this has read the store's lock directory.
    ///
    /// One read per enumeration point, never one per session: the same shape
    /// the sidecar axes are swept with, and the number the `locked` axis's
    /// cost budget is about.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn lock_directory_reads(&self) -> u64 {
        self.lock_directory_reads.load(Ordering::Relaxed)
    }

    /// How many locks this has probed.
    ///
    /// The cost that the holder record filters, and the one a byte budget
    /// cannot see: a probe transfers nothing. A settled store's sweep leaves
    /// this where it was.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn lock_probes(&self) -> u64 {
        self.lock_probes.load(Ordering::Relaxed)
    }

    /// Whether the store holds a session log for `id`.
    ///
    /// The membership test materialization gates on. It costs one `stat`, so
    /// it says nothing about how many sessions the store holds: that is what
    /// the id grammar buys
    /// ([`crate::host::validate_session_id`]).
    ///
    /// A log this store cannot stat is a failure rather than an absence, so
    /// a store nothing can read refuses a request loudly instead of reporting
    /// every session in it as gone.
    ///
    /// NOTE(aljoscha): a `stat` answers under the filesystem's own name
    /// matching, where an enumeration answered under exact string equality.
    /// On a case-insensitive filesystem `ABC` therefore now finds `abc.jsonl`,
    /// and materializing under the id as spelled would put the same log in the
    /// directory twice, once live and once cold, until the next enumeration
    /// drops the alias. Unreachable through a client that only ever echoes ids
    /// the directory gave it.
    pub(crate) fn contains(&self, id: &str) -> Result<bool, ConversationError> {
        self.membership_lookups.fetch_add(1, Ordering::Relaxed);
        Ok(self.store.session_metadata(id)?.is_some())
    }

    /// Record what the host knows about a session it just released, so a
    /// refresh serves it without an enumeration.
    ///
    /// Touches no filesystem. The row carries its own consistency (see
    /// [`ReleasedRow`]): the fingerprint is the file state the release left
    /// behind. It is also the state the next enumeration finds, unless a rival
    /// writer took the freed lock and appended in between.
    pub(crate) fn note_released(&self, released: &ReleasedRow) {
        let ReleasedRow {
            file,
            last_activity,
            tag,
            archived,
        } = released;
        let at = fingerprint(file);
        let mut cache = self.cache();
        cache.rows.insert(
            file.session_id.clone(),
            Row {
                at,
                last_activity: *last_activity,
            },
        );
        cache
            .tags
            .insert(file.session_id.clone(), Arc::new(tag.clone()));
        // The bit the driver held, including a cleared bit: an entry saying
        // the session is not archived is how a release states what it did
        // under the session's lock, which a sidecar listing taken before that
        // write must not undo (see [`Self::record_archived`]).
        cache.archived.insert(file.session_id.clone(), *archived);
    }

    /// Read a cold label without holding the directory lock. Only replace the
    /// publication this scan started with, so a concurrent release wins.
    ///
    /// A failed read leaves the known label alone rather than recording
    /// "untagged": the failure says nothing about the label.
    fn tag(&self, session_id: &str, before: Option<&Arc<Option<String>>>) {
        let tag = match self.store.read_tag(session_id) {
            Ok(tag) => tag,
            Err(err) => {
                tracing::warn!(
                    session = session_id,
                    "could not read a session's tag: {err}"
                );
                return;
            }
        };
        let mut cache = self.cache();
        let unchanged = match (cache.tags.get(session_id), before) {
            (Some(held), Some(before)) => Arc::ptr_eq(held, before),
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            cache.tags.insert(session_id.to_string(), Arc::new(tag));
        }
    }

    /// Drop what we hold for sessions the store no longer holds, so the cache
    /// stays a projection of the directory rather than of its history.
    ///
    /// Only ids in `known`, which this scan read the directory after taking, are
    /// eligible. An id that arrived while the scan ran was recorded by something
    /// that knew more about it than this scan's directory read did: a newer
    /// enumeration, or a release handing over the state it read under the
    /// session's own lock. Evicting one of those would undo it.
    fn evict(&self, enumerated: &[SessionMetadata], known: &HashSet<String>) {
        let present: HashSet<&str> = enumerated
            .iter()
            .map(|metadata| metadata.session_id.as_str())
            .collect();
        let gone = |id: &String| known.contains(id) && !present.contains(id.as_str());
        let mut cache = self.cache();
        cache.rows.retain(|id, _| !gone(id));
    }

    /// Clear labels whose sidecars are gone, without undoing publications that
    /// arrived during the scan. Keep the clear while its session exists: an
    /// older scan may have started with no label and still be reading one.
    /// Removing the clear would let that scan mistake absence for permission
    /// to publish its stale read.
    fn clear_missing_tags(
        &self,
        sidecars: &[SidecarMetadata],
        sessions: &[SessionMetadata],
        labelled: &HashMap<String, Arc<Option<String>>>,
    ) {
        let present: HashSet<&str> = sidecars
            .iter()
            .map(|sidecar| sidecar.session_id.as_str())
            .collect();
        let sessions: HashSet<&str> = sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect();
        self.cache().tags.retain(|id, held| {
            if !present.contains(id.as_str())
                && labelled
                    .get(id)
                    .is_some_and(|before| Arc::ptr_eq(before, held))
            {
                if !sessions.contains(id.as_str()) {
                    return false;
                }
                *held = Arc::new(None);
            }
            true
        });
    }

    /// Fold the archived listing into the cache: the sidecars it found say
    /// their sessions are archived, and the entries whose sidecars are gone
    /// leave.
    ///
    /// One pass under one guard, where the label needs two, because this axis
    /// opens no file: the listing is the entire answer, so there is no read to
    /// keep out of the lock.
    ///
    /// `filed` is what the cache held before this scan read anything, and both
    /// halves are eligibility rather than truth. An entry that is not what the
    /// scan started with was published by something that knew more than the
    /// listing does, a release recording the bit its driver held under the
    /// session's own lock, and the scan neither overwrites nor evicts it. The
    /// bit has to be compared, not just the entry's presence: a listing
    /// reports only the sidecars that exist, so without the release's own
    /// `false` an unarchive that landed mid-scan would read as an id the cache
    /// never held and be quietly re-archived until the next enumeration point.
    ///
    /// The comparison catches a republished entry, it does not prove one is
    /// absent. A session archived and unarchived again while the scan ran
    /// leaves the value it started at and is written from the listing, which
    /// stands until the next enumeration point. The window is the tail of one
    /// `readdir` and no test can reach it; recording a generation per entry is
    /// what closing it would cost, for a race between a scan and two commands
    /// on one session.
    ///
    /// A live session's entry is written like any other. Nothing reads it
    /// while the session is live, a directory answers a live row from the
    /// host's own status, and its release overwrites the entry with what its
    /// driver held.
    fn record_archived(&self, sidecars: &[SidecarMetadata], filed: &HashMap<String, bool>) {
        let present: HashSet<&str> = sidecars
            .iter()
            .map(|sidecar| sidecar.session_id.as_str())
            .collect();
        let mut cache = self.cache();
        let gone = |id: &String, held: &bool| {
            filed.get(id) == Some(held) && !present.contains(id.as_str())
        };
        cache.archived.retain(|id, held| !gone(id, held));
        for sidecar in sidecars {
            if cache.archived.get(&sidecar.session_id) != filed.get(&sidecar.session_id) {
                continue;
            }
            cache.archived.insert(sidecar.session_id.clone(), true);
        }
    }

    fn enumerate_store(&self) -> Result<Vec<SessionMetadata>, ConversationError> {
        self.directory_reads.fetch_add(1, Ordering::Relaxed);
        self.store.list_sessions()
    }

    fn enumerate_sidecars(&self) -> Result<Vec<SidecarMetadata>, ConversationError> {
        self.sidecar_directory_reads.fetch_add(1, Ordering::Relaxed);
        self.store.enumerate_tags()
    }

    fn enumerate_archived_sidecars(&self) -> Result<Vec<SidecarMetadata>, ConversationError> {
        self.sidecar_directory_reads.fetch_add(1, Ordering::Relaxed);
        self.store.enumerate_archived()
    }

    fn enumerate_locks(&self) -> Result<Vec<LockMetadata>, ConversationError> {
        self.lock_directory_reads.fetch_add(1, Ordering::Relaxed);
        self.store.enumerate_locks()
    }

    /// Ask the filesystem whether a rival holds one session's lock.
    ///
    /// Counted, because a probe transfers no bytes and no budget can see it.
    pub(crate) fn probe_lock(&self, session_id: &str) -> Result<bool, ConversationError> {
        self.lock_probes.fetch_add(1, Ordering::Relaxed);
        self.store.probe_lock(session_id)
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().expect("cold session cache poisoned")
    }
}

fn fingerprint(metadata: &SessionMetadata) -> Fingerprint {
    Fingerprint::of(metadata.modified_at, metadata.size_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store whose directory and metadata the tests can edit independently.
    #[derive(Default)]
    struct FakeStore {
        files: StdMutex<Vec<FakeFile>>,
        sidecars: StdMutex<Vec<FakeSidecar>>,
        /// The archived sidecars, by id and modification time. No contents:
        /// the file's existence is the bit, so this axis has no read at all.
        archived: StdMutex<Vec<(String, i64)>>,
        /// Set to fail the read of the sidecar directory, as a permission
        /// change on `meta/` does.
        sidecars_unreadable: StdMutex<bool>,
        /// Run once after a session listing has been captured, so a test can
        /// act while a scan is between its directory read and its eviction.
        during_session_listing: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
        /// The same, inside the first sidecar read of a scan, which is after
        /// the sidecar listing was taken and before the labels are evicted.
        during_tag_read: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
        /// The same, inside the archived listing and after it was taken, which
        /// is the only window that axis has: it reads no file, so a release
        /// landing here is one the scan's listing cannot know about.
        during_archived_listing: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
        /// The store's lock files. The listing reports
        /// [`FakeLock::has_holder_record`] and a probe answers
        /// [`FakeLock::held`], so all four combinations are reachable: a rival
        /// holding one, a record a crash left on a free lock, a holder that
        /// never wrote its record, and a settled lock.
        locks: StdMutex<Vec<FakeLock>>,
        /// Set to fail the read of the lock directory, as a permission change
        /// on `locks/` does.
        locks_unreadable: StdMutex<bool>,
        /// The ids a probe was asked about, in order.
        probes: StdMutex<Vec<String>>,
        /// Run once inside the lock listing and after it was taken, the window
        /// a refusal can land in while a sweep is between its listing and its
        /// probes.
        during_lock_listing: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    #[derive(Clone)]
    struct FakeLock {
        id: String,
        has_holder_record: bool,
        held: bool,
    }

    /// One log in the fake store. `modified` and `size` are independent, as
    /// they are on a real filesystem.
    #[derive(Clone)]
    struct FakeFile {
        id: String,
        /// Epoch seconds. Also the row's activity stamp, since that is the
        /// file's modification time.
        modified: i64,
        size: u64,
    }

    /// One tag sidecar, independent of the session log.
    #[derive(Clone)]
    struct FakeSidecar {
        id: String,
        /// Epoch seconds, the sidecar's own modification time.
        modified: i64,
        /// What the sidecar reads as, `None` for one that says nothing usable.
        tag: Option<String>,
        /// Whether the store can read the file at all. A sidecar that cannot
        /// be read is a different answer from one that reads as untagged, and
        /// the cache is only allowed to remember the second.
        readable: bool,
    }

    impl FakeSidecar {
        /// The file size reported by the listing.
        fn size(&self) -> u64 {
            self.tag.as_ref().map_or(0, |tag| {
                u64::try_from(tag.len()).expect("a label fits a u64")
            })
        }
    }

    impl FakeFile {
        fn new(id: &str, modified: i64) -> Self {
            Self {
                id: id.to_string(),
                modified,
                size: 100,
            }
        }
    }

    impl SessionStore for FakeStore {
        fn list_sessions(&self) -> Result<Vec<SessionMetadata>, ConversationError> {
            // `list` preserves the store's order, and the real one is
            // latest-first. Ascending here, so assertions read in order.
            let mut files = self.files.lock().expect("files").clone();
            if let Some(interleave) = self.during_session_listing.lock().expect("hook").take() {
                interleave();
            }
            files.sort_by(|left, right| left.id.cmp(&right.id));
            Ok(files
                .iter()
                .map(|file| SessionMetadata::new(file.id.clone(), at(file.modified), file.size))
                .collect())
        }

        fn session_metadata(
            &self,
            session_id: &str,
        ) -> Result<Option<SessionMetadata>, ConversationError> {
            // A `stat` of one path: it does not touch the directory, which is
            // what the `directory_reads` assertions rest on.
            Ok(self
                .file(session_id)
                .map(|file| SessionMetadata::new(file.id, at(file.modified), file.size)))
        }

        fn enumerate_tags(&self) -> Result<Vec<SidecarMetadata>, ConversationError> {
            if *self.sidecars_unreadable.lock().expect("readable") {
                return Err(std::io::Error::other("meta/ is not readable").into());
            }
            let mut sidecars = self.sidecars.lock().expect("sidecars").clone();
            sidecars.sort_by(|left, right| left.id.cmp(&right.id));
            Ok(sidecars
                .iter()
                .map(|sidecar| SidecarMetadata {
                    session_id: sidecar.id.clone(),
                    modified_at: at(sidecar.modified),
                    size_bytes: sidecar.size(),
                })
                .collect())
        }

        fn enumerate_locks(&self) -> Result<Vec<LockMetadata>, ConversationError> {
            if *self.locks_unreadable.lock().expect("readable") {
                return Err(std::io::Error::other("locks/ is not readable").into());
            }
            let mut locks = self.locks.lock().expect("locks").clone();
            locks.sort_by(|left, right| left.id.cmp(&right.id));
            let listed: Vec<LockMetadata> = locks
                .iter()
                .map(|lock| LockMetadata {
                    session_id: lock.id.clone(),
                    has_holder_record: lock.has_holder_record,
                })
                .collect();
            if let Some(interleave) = self.during_lock_listing.lock().expect("hook").take() {
                interleave();
            }
            Ok(listed)
        }

        fn probe_lock(&self, session_id: &str) -> Result<bool, ConversationError> {
            self.probes
                .lock()
                .expect("probes")
                .push(session_id.to_string());
            Ok(self
                .locks
                .lock()
                .expect("locks")
                .iter()
                .any(|lock| lock.id == session_id && lock.held))
        }

        fn read_tag(&self, session_id: &str) -> Result<Option<String>, ConversationError> {
            let sidecar = self.sidecar(session_id);
            if sidecar.as_ref().is_some_and(|sidecar| !sidecar.readable) {
                return Err(std::io::Error::other("the sidecar is not readable").into());
            }
            // Captured before the hook runs, as a real read is: the hook then
            // stands for everything that happens while the read is in flight.
            let read = sidecar.and_then(|sidecar| sidecar.tag);
            if let Some(interleave) = self.during_tag_read.lock().expect("hook").take() {
                interleave();
            }
            Ok(read)
        }

        fn enumerate_archived(&self) -> Result<Vec<SidecarMetadata>, ConversationError> {
            if *self.sidecars_unreadable.lock().expect("readable") {
                return Err(std::io::Error::other("meta/ is not readable").into());
            }
            let mut archived = self.archived.lock().expect("archived").clone();
            archived.sort();
            let listed: Vec<SidecarMetadata> = archived
                .iter()
                .map(|(id, modified)| SidecarMetadata {
                    session_id: id.clone(),
                    modified_at: at(*modified),
                    // An archived sidecar is empty: its existence is the bit.
                    size_bytes: 0,
                })
                .collect();
            if let Some(interleave) = self.during_archived_listing.lock().expect("hook").take() {
                interleave();
            }
            Ok(listed)
        }
    }

    impl FakeStore {
        fn file(&self, id: &str) -> Option<FakeFile> {
            self.files
                .lock()
                .expect("files")
                .iter()
                .find(|file| file.id == id)
                .cloned()
        }

        fn sidecar(&self, id: &str) -> Option<FakeSidecar> {
            self.sidecars
                .lock()
                .expect("sidecars")
                .iter()
                .find(|sidecar| sidecar.id == id)
                .cloned()
        }

        /// Label `id` at sidecar modification time `modified`, as a tag
        /// command's atomic rewrite would.
        fn tag(&self, id: &str, tag: &str, modified: i64) {
            self.write_sidecar(FakeSidecar {
                id: id.to_string(),
                modified,
                tag: Some(tag.to_string()),
                readable: true,
            });
        }

        /// Whether `id`'s sidecar can be read.
        fn sidecar_readable(&self, id: &str, readable: bool) {
            let mut sidecars = self.sidecars.lock().expect("sidecars");
            let sidecar = sidecars
                .iter_mut()
                .find(|sidecar| sidecar.id == id)
                .expect("a sidecar to edit");
            sidecar.readable = readable;
        }

        fn write_sidecar(&self, sidecar: FakeSidecar) {
            let mut sidecars = self.sidecars.lock().expect("sidecars");
            sidecars.retain(|held| held.id != sidecar.id);
            sidecars.push(sidecar);
        }

        fn sidecars_unreadable(&self, unreadable: bool) {
            *self.sidecars_unreadable.lock().expect("readable") = unreadable;
        }

        /// Clear `id`'s label, which removes its sidecar.
        fn untag(&self, id: &str) {
            self.sidecars
                .lock()
                .expect("sidecars")
                .retain(|sidecar| sidecar.id != id);
        }

        /// Put a log last written at `modified` in the store.
        fn put(&self, id: &str, modified: i64) {
            self.write(FakeFile::new(id, modified));
        }

        fn write(&self, file: FakeFile) {
            let mut files = self.files.lock().expect("files");
            files.retain(|held| held.id != file.id);
            files.push(file);
        }

        /// Change one log in place, leaving its fingerprint to the caller.
        fn edit(&self, id: &str, edit: impl FnOnce(&mut FakeFile)) {
            let mut files = self.files.lock().expect("files");
            let file = files
                .iter_mut()
                .find(|file| file.id == id)
                .expect("a file to edit");
            edit(file);
        }

        fn during_session_listing(&self, interleave: impl FnOnce() + Send + 'static) {
            *self.during_session_listing.lock().expect("hook") = Some(Box::new(interleave));
        }

        fn during_tag_read(&self, interleave: impl FnOnce() + Send + 'static) {
            *self.during_tag_read.lock().expect("hook") = Some(Box::new(interleave));
        }

        fn during_archived_listing(&self, interleave: impl FnOnce() + Send + 'static) {
            *self.during_archived_listing.lock().expect("hook") = Some(Box::new(interleave));
        }

        /// Archive `id` at sidecar modification time `modified`, as the
        /// archive command's create does.
        fn archive(&self, id: &str, modified: i64) {
            let mut archived = self.archived.lock().expect("archived");
            archived.retain(|(held, _)| held != id);
            archived.push((id.to_string(), modified));
        }

        /// Unarchive `id`, which removes its sidecar.
        /// Put a lock file in the store: `record` is what a `stat` shows and
        /// `held` is what a probe answers, which are independent in the field.
        fn lock(&self, id: &str, record: bool, held: bool) {
            let mut locks = self.locks.lock().expect("locks");
            locks.retain(|lock| lock.id != id);
            locks.push(FakeLock {
                id: id.to_string(),
                has_holder_record: record,
                held,
            });
        }

        /// A rival lets go cleanly: the record is truncated while the lock is
        /// still held, and the lock frees.
        fn release(&self, id: &str) {
            self.lock(id, false, false);
        }

        fn locks_unreadable(&self, unreadable: bool) {
            *self.locks_unreadable.lock().expect("readable") = unreadable;
        }

        fn during_lock_listing(&self, interleave: impl FnOnce() + Send + 'static) {
            *self.during_lock_listing.lock().expect("hook") = Some(Box::new(interleave));
        }

        fn probes(&self) -> Vec<String> {
            self.probes.lock().expect("probes").clone()
        }

        fn unarchive(&self, id: &str) {
            self.archived
                .lock()
                .expect("archived")
                .retain(|(held, _)| held != id);
        }

        fn remove(&self, id: &str) {
            self.files
                .lock()
                .expect("files")
                .retain(|file| file.id != id);
        }
    }

    /// Epoch seconds as the wall clock a row reports.
    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::UNIX_EPOCH + chrono::Duration::seconds(seconds)
    }

    /// The listed ids paired with their activity stamps, for compact
    /// assertions. The stamp is the whole payload of a cold row, so every
    /// assertion here is also an assertion about where it came from.
    fn listed(cold: Vec<ColdSession>) -> Vec<(String, i64)> {
        let mut rows: Vec<(String, i64)> = cold
            .into_iter()
            .map(|session| (session.id, session.last_activity.timestamp()))
            .collect();
        rows.sort();
        rows
    }

    /// An enumeration point followed by a refresh, which is the pair every
    /// assertion about what the store holds goes through.
    fn refreshed(
        cold: &ColdSessions<FakeStore>,
        live: impl Fn(&str) -> bool,
    ) -> Vec<(String, i64)> {
        cold.enumerate(live).expect("enumerate");
        listed(cold.rows())
    }

    /// A released session's row, as its driver would hand it over. The file
    /// the release left behind, with the stamp the driver settled on.
    fn released(id: &str, modified: i64, size: u64) -> ReleasedRow {
        ReleasedRow {
            file: SessionMetadata::new(id.to_string(), at(modified), size),
            last_activity: at(modified),
            tag: None,
            archived: false,
        }
    }

    /// The labels the rows carry, paired with their ids.
    fn labelled(cold: Vec<ColdSession>) -> Vec<(String, Option<String>)> {
        let mut rows: Vec<(String, Option<String>)> = cold
            .into_iter()
            .map(|session| (session.id, session.tag))
            .collect();
        rows.sort();
        rows
    }

    /// The locked bits the rows carry, paired with their ids.
    fn barred(cold: Vec<ColdSession>) -> Vec<(String, bool)> {
        let mut rows: Vec<(String, bool)> = cold
            .into_iter()
            .map(|session| (session.id, session.locked))
            .collect();
        rows.sort();
        rows
    }

    /// The archived bits the rows carry, paired with their ids.
    fn filed(cold: Vec<ColdSession>) -> Vec<(String, bool)> {
        let mut rows: Vec<(String, bool)> = cold
            .into_iter()
            .map(|session| (session.id, session.archived))
            .collect();
        rows.sort();
        rows
    }

    /// A row's stamp is the log file's modification time, and it follows the
    /// file: a log a sibling process appends to reports the append at the next
    /// enumeration point, without the row being read.
    #[test]
    fn a_rows_stamp_is_the_files_modification_time() {
        let store = FakeStore::default();
        store.put("a", 1_700_000_000);
        let cold = ColdSessions::new(store);
        assert_eq!(
            refreshed(&cold, |_| false),
            [("a".to_string(), 1_700_000_000)],
        );

        cold.store.edit("a", |file| {
            file.modified += 60;
            file.size += 400;
        });
        assert_eq!(
            refreshed(&cold, |_| false),
            [("a".to_string(), 1_700_000_060)],
            "the stamp moves with the file",
        );
    }

    /// A log that appears is picked up and one that vanishes drops out, both
    /// at the next enumeration point.
    #[test]
    fn an_appearing_log_is_listed_and_a_vanished_one_is_forgotten() {
        let store = FakeStore::default();
        store.put("a", 2);
        let cold = ColdSessions::new(store);
        assert_eq!(refreshed(&cold, |_| false), [("a".to_string(), 2)]);

        cold.store.put("b", 4);
        assert_eq!(
            refreshed(&cold, |_| false),
            [("a".to_string(), 2), ("b".to_string(), 4)],
        );

        cold.store.remove("a");
        assert_eq!(refreshed(&cold, |_| false), [("b".to_string(), 4)]);
    }

    /// A vanished log takes its row with it.
    ///
    /// A row is pinned against the fingerprint it describes, so one that
    /// outlived its file would go on answering for whatever file takes the id
    /// next, and a recycled id landing on the same `(mtime, size)` would never
    /// dislodge it. The stamp here is deliberately one no `stat` could produce
    /// for either file, which is the only way to tell a surviving row from a
    /// freshly derived one.
    #[test]
    fn a_vanished_log_leaves_no_row_behind() {
        let store = FakeStore::default();
        store.put("a", 2);
        let cold = ColdSessions::new(store);
        // Materialized and released, so the host recorded a row of its own.
        cold.note_released(&ReleasedRow {
            file: SessionMetadata::new("a".to_string(), at(2), 100),
            last_activity: at(99),
            tag: None,
            archived: false,
        });
        assert_eq!(listed(cold.rows()), [("a".to_string(), 99)]);

        cold.store.remove("a");
        assert_eq!(refreshed(&cold, |_| false), []);

        // A different file, same id, and the fingerprint the old one had.
        cold.store.put("a", 2);
        assert_eq!(
            refreshed(&cold, |_| false),
            [("a".to_string(), 2)],
            "the row outlived the file that produced it",
        );
    }

    /// A refresh serves what the last enumeration point found and goes nowhere
    /// near the store, so a file a sibling process leaves in the directory is
    /// invisible until something enumerates.
    #[test]
    fn a_refresh_serves_the_rows_without_reading_the_directory() {
        let store = FakeStore::default();
        store.put("a", 2);
        let cold = ColdSessions::new(store);
        assert_eq!(refreshed(&cold, |_| false), [("a".to_string(), 2)]);
        assert_eq!(cold.directory_reads(), 1);

        cold.store.put("sibling", 9);
        for _ in 0..10 {
            assert_eq!(
                listed(cold.rows()),
                [("a".to_string(), 2)],
                "the refresh reports what the enumeration found",
            );
        }
        assert_eq!(
            cold.directory_reads(),
            1,
            "and ten refreshes read the directory no times",
        );

        assert_eq!(
            refreshed(&cold, |_| false),
            [("a".to_string(), 2), ("sibling".to_string(), 9)],
            "the next enumeration point picks the file up",
        );
    }

    /// A released session is served from what the host recorded, so its row
    /// costs neither an enumeration nor a read of the log it just closed.
    #[test]
    fn a_released_session_is_served_without_an_enumeration() {
        let store = FakeStore::default();
        store.put("held", 4);
        let cold = ColdSessions::new(store);
        // Held live, so the enumeration leaves it out.
        assert_eq!(refreshed(&cold, |id| id == "held"), []);

        cold.note_released(&released("held", 4, 400));
        assert_eq!(listed(cold.rows()), [("held".to_string(), 4)]);
        assert_eq!(cold.directory_reads(), 1, "the release read nothing");
    }

    /// The row a release records carries what the driver saw, not what the
    /// file says, and a later enumeration finding the same file leaves it
    /// alone. Replacing it with the `stat` would undo the release one tick
    /// later, and the two answer different questions (see [`ReleasedRow`]).
    #[test]
    fn a_release_records_the_drivers_stamp_not_the_files() {
        let store = FakeStore::default();
        store.write(FakeFile {
            size: 400,
            ..FakeFile::new("held", 10)
        });
        let cold = ColdSessions::new(store);
        cold.note_released(&ReleasedRow {
            file: SessionMetadata::new("held".to_string(), at(10), 400),
            last_activity: at(14),
            tag: None,
            archived: false,
        });
        assert_eq!(listed(cold.rows()), [("held".to_string(), 14)]);

        assert_eq!(
            refreshed(&cold, |_| false),
            [("held".to_string(), 14)],
            "an enumeration over the unmoved file keeps the release's stamp",
        );

        // Once the file moves, the stat is the better answer again.
        cold.store.edit("held", |file| file.modified = 20);
        assert_eq!(refreshed(&cold, |_| false), [("held".to_string(), 20)]);
    }

    /// A live session's row is left alone by an enumeration rather than
    /// dropped or rebuilt. The host's live snapshot can predate a release, and
    /// the row the release recorded must not be undone by a scan that still
    /// believes the session is held.
    #[test]
    fn an_enumeration_leaves_a_live_sessions_row_alone() {
        let store = FakeStore::default();
        store.put("a", 2);
        let cold = ColdSessions::new(store);
        assert_eq!(refreshed(&cold, |_| false), [("a".to_string(), 2)]);

        // Materialized, appended to, and released, all while a scan that
        // snapshotted the live set beforehand is still running. The store's
        // own view of the file is deliberately left behind at 2, so the row
        // can only read 5 if the release's record survived.
        cold.note_released(&released("a", 5, 800));
        assert_eq!(refreshed(&cold, |id| id == "a"), [("a".to_string(), 5)]);
    }

    /// A scan may only evict rows it could have seen. A row that arrives while
    /// a scan runs was recorded by something that knew more about that session
    /// than the scan's directory read did, and a release recording the state it
    /// read under the session's own lock is exactly that.
    #[test]
    fn a_scan_does_not_evict_a_row_that_arrived_while_it_ran() {
        let store = FakeStore::default();
        store.put("a", 2);
        let cold = Arc::new(ColdSessions::new(store));
        let releasing = Arc::downgrade(&cold);
        cold.store.during_session_listing(move || {
            let cold = releasing.upgrade().expect("the cache outlives the scan");
            // A session whose log the scan's directory read never saw, because
            // it was created after it.
            cold.note_released(&ReleasedRow {
                tag: Some("late label".to_string()),
                ..released("late", 7, 400)
            });
        });

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            listed(cold.rows()),
            [("a".to_string(), 2), ("late".to_string(), 7)],
            "the scan evicted a row it never had a view of",
        );
        assert_eq!(
            labelled(cold.rows()),
            [
                ("a".to_string(), None),
                ("late".to_string(), Some("late label".to_string())),
            ],
            "and the label that came with it, which no sidecar list can \
             account for",
        );
    }

    /// The membership test answers off one `stat`. It never reads the
    /// directory, which is what makes it independent of how many sessions the
    /// store holds.
    #[test]
    fn membership_answers_off_one_stat() {
        let store = FakeStore::default();
        store.put("a", 3);
        let cold = ColdSessions::new(store);

        for _ in 0..5 {
            assert!(cold.contains("a").expect("the store answered"));
            assert!(!cold.contains("nobody").expect("the store answered"));
        }
        assert_eq!(
            cold.directory_reads(),
            0,
            "ten membership questions and not one directory read",
        );
    }

    /// A store where nothing is labelled never opens a sidecar, however often
    /// it is enumerated. Untagged is the common case, and an implementation
    /// that asked per session would turn it into a read per row.
    #[test]
    fn an_untagged_store_reads_no_sidecar() {
        let store = FakeStore::default();
        for id in ["a", "b", "c"] {
            store.put(id, 5);
        }
        let cold = ColdSessions::new(store);
        cold.store
            .during_tag_read(|| panic!("an untagged store read a sidecar"));

        for _ in 0..5 {
            cold.enumerate(|_| false).expect("enumerate");
            assert_eq!(
                labelled(cold.rows()),
                [
                    ("a".to_string(), None),
                    ("b".to_string(), None),
                    ("c".to_string(), None),
                ],
            );
        }
    }

    /// A label follows its sidecar, not its log: clearing a tag removes the
    /// file, and the row stays in the directory having lost only its label.
    #[test]
    fn a_cleared_label_leaves_the_row_behind() {
        let store = FakeStore::default();
        store.put("a", 5);
        store.tag("a", "fix-auth", 6);
        let cold = ColdSessions::new(store);
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            labelled(cold.rows()),
            [("a".to_string(), Some("fix-auth".to_string()))],
        );

        cold.store.untag("a");
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(labelled(cold.rows()), [("a".to_string(), None)]);
        assert_eq!(
            listed(cold.rows()),
            [("a".to_string(), 5)],
            "the session is still in the directory",
        );
    }

    /// A session the host holds live answers its own label out of memory, so
    /// an enumeration does not read its sidecar. The file can only be staler
    /// than what the driver holds, and its release hands the label over.
    #[test]
    fn a_live_sessions_sidecar_is_not_read() {
        let store = FakeStore::default();
        store.put("live", 5);
        store.tag("live", "on disk", 6);
        let cold = ColdSessions::new(store);

        cold.store
            .during_tag_read(|| panic!("a live label was read from disk"));
        for _ in 0..3 {
            cold.enumerate(|id| id == "live").expect("enumerate");
        }

        // Released with the label the driver held, which is what the row
        // carries: no enumeration has read the file at all.
        cold.note_released(&ReleasedRow {
            file: SessionMetadata::new("live".to_string(), at(5), 100),
            last_activity: at(9),
            tag: Some("in memory".to_string()),
            archived: false,
        });
        assert_eq!(
            labelled(cold.rows()),
            [("live".to_string(), Some("in memory".to_string()))],
        );
        // A cold label is read from disk at the next enumeration point.
        cold.store.during_tag_read(|| {});
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            labelled(cold.rows()),
            [("live".to_string(), Some("on disk".to_string()))],
        );
    }

    /// A release that hands over no label removes the one the cache held: the
    /// driver's answer is the current one either way, and a cleared tag would
    /// otherwise keep showing on the row until the next enumeration.
    #[test]
    fn a_release_without_a_label_clears_the_cached_one() {
        let store = FakeStore::default();
        store.put("a", 5);
        store.tag("a", "fix-auth", 6);
        let cold = ColdSessions::new(store);
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            labelled(cold.rows()),
            [("a".to_string(), Some("fix-auth".to_string()))],
        );

        // Held live, cleared, and released. The sidecar is gone with the
        // clear, as the tag command's write leaves it.
        cold.store.untag("a");
        cold.note_released(&released("a", 5, 100));
        assert_eq!(labelled(cold.rows()), [("a".to_string(), None)]);
    }

    /// A label a release published while a scan ran survives that scan.
    ///
    /// The rule [`ColdSessions::evict`] states for rows, on the map where the
    /// id alone cannot state it: a label arrives on a session the cache
    /// already holds, so the scan has to recognise the entry itself rather
    /// than the id it sits under.
    #[test]
    fn a_scan_does_not_evict_a_label_that_arrived_while_it_ran() {
        let store = FakeStore::default();
        store.put("a", 5);
        store.put("held", 5);
        store.tag("a", "fix-auth", 6);
        let cold = Arc::new(ColdSessions::new(store));
        // A first scan, so both sessions have rows and "a" has a label.
        cold.enumerate(|_| false).expect("enumerate");

        // "a" is relabelled, which is what gives the next scan a sidecar to
        // read and so a window for the release below to land in.
        cold.store.tag("a", "fix-auth-again", 7);
        let releasing = Arc::downgrade(&cold);
        cold.store.during_tag_read(move || {
            let cold = releasing.upgrade().expect("the cache outlives the scan");
            // "held" was labelled while it was live and released with the
            // label its driver held. Its sidecar is on disk too, but the scan
            // took its listing before the file existed.
            cold.store.tag("held", "live label", 9);
            cold.note_released(&ReleasedRow {
                tag: Some("live label".to_string()),
                ..released("held", 5, 100)
            });
        });

        // The scan believes "held" is live, which is what it was when the
        // live set was snapshotted.
        cold.enumerate(|id| id == "held").expect("enumerate");
        assert_eq!(
            labelled(cold.rows()),
            [
                // Also what says the scan really did read a sidecar, which is
                // the window the release had to land in.
                ("a".to_string(), Some("fix-auth-again".to_string())),
                ("held".to_string(), Some("live label".to_string())),
            ],
            "the label the release handed over outlived the scan",
        );
    }

    /// The same rule for a label the cache did hold when the scan started: a
    /// release that replaced it while the scan ran is not the entry the scan
    /// looked at, so the scan's listing does not get to evict it.
    #[test]
    fn a_scan_does_not_evict_a_label_a_release_replaced() {
        let store = FakeStore::default();
        store.put("a", 5);
        store.put("b", 5);
        store.tag("a", "old", 6);
        store.tag("b", "b-label", 6);
        let cold = Arc::new(ColdSessions::new(store));
        cold.enumerate(|_| false).expect("enumerate");

        // "a" is cleared, so the next scan's listing will not carry it, and
        // "b" is relabelled, which gives that scan a sidecar to read.
        cold.store.untag("a");
        cold.store.tag("b", "b-again", 7);
        let releasing = Arc::downgrade(&cold);
        cold.store.during_tag_read(move || {
            let cold = releasing.upgrade().expect("the cache outlives the scan");
            // "a" was materialized and relabelled after the listing was
            // taken, then released with the label its driver held.
            cold.store.tag("a", "re-tagged", 9);
            cold.note_released(&ReleasedRow {
                tag: Some("re-tagged".to_string()),
                ..released("a", 5, 100)
            });
        });

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            labelled(cold.rows()),
            [
                ("a".to_string(), Some("re-tagged".to_string())),
                ("b".to_string(), Some("b-again".to_string())),
            ],
            "the scan evicted a label that is not the one it looked at",
        );
    }

    /// A release outranks an in-flight read, including a repeated label or
    /// clear. Comparing only text would miss those publications.
    #[test]
    fn a_release_during_a_sidecar_read_outranks_what_the_scan_read() {
        for (initial, tag) in [
            (None, Some("new")),
            (Some("old"), Some("old")),
            (None, None),
        ] {
            let store = FakeStore::default();
            store.put("a", 5);
            if let Some(initial) = initial {
                store.tag("a", initial, 6);
            }
            let cold = Arc::new(ColdSessions::new(store));
            cold.enumerate(|_| false).expect("enumerate");
            assert_eq!(cold.label("a").as_deref(), initial);
            cold.store.tag("a", "stale", 7);

            let releasing = Arc::downgrade(&cold);
            cold.store.during_tag_read(move || {
                let cold = releasing.upgrade().expect("the cache outlives the scan");
                match tag {
                    Some(tag) => cold.store.tag("a", tag, 8),
                    None => cold.store.untag("a"),
                }
                cold.note_released(&ReleasedRow {
                    tag: tag.map(str::to_string),
                    ..released("a", 5, 100)
                });
                if tag.is_none() {
                    // A second scan sees the missing sidecar before the
                    // in-flight read returns. It must preserve the clear's
                    // precedence over that read, not turn it into absence.
                    cold.enumerate(|_| false).expect("overlapping scan");
                }
            });

            cold.enumerate(|_| false).expect("enumerate");
            assert_eq!(
                labelled(cold.rows()),
                [("a".to_string(), tag.map(str::to_string))],
                "the scan must not overwrite a release of {tag:?}",
            );
            cold.enumerate(|_| false).expect("enumerate again");
            assert_eq!(cold.label("a").as_deref(), tag);
        }
    }

    /// A sidecar the store cannot read leaves the cache alone rather than
    /// recording "untagged". Once the file can be read again, enumeration
    /// picks up its current label.
    #[test]
    fn an_unreadable_sidecar_leaves_the_cached_label_alone() {
        let store = FakeStore::default();
        store.put("a", 5);
        store.tag("a", "fix-auth", 6);
        let cold = ColdSessions::new(store);
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            labelled(cold.rows()),
            [("a".to_string(), Some("fix-auth".to_string()))],
        );

        // An unreadable replacement does not erase the known label.
        cold.store.tag("a", "relabelled", 7);
        cold.store.sidecar_readable("a", false);
        for _ in 0..3 {
            cold.enumerate(|_| false).expect("enumerate");
            assert_eq!(
                labelled(cold.rows()),
                [("a".to_string(), Some("fix-auth".to_string()))],
                "the label we had stands over a read that failed",
            );
        }

        cold.store.sidecar_readable("a", true);
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            labelled(cold.rows()),
            [("a".to_string(), Some("relabelled".to_string()))],
            "and the file answers again once it can be read",
        );
    }

    /// A refresh reads neither directory, the sidecar listings included. All
    /// three are enumeration work, and none transfers bytes a read budget
    /// could see, so the counts are the only seam that catches a refresh going
    /// looking.
    #[test]
    fn a_refresh_reads_neither_directory() {
        let store = FakeStore::default();
        store.put("a", 2);
        store.tag("a", "fix-auth", 3);
        store.archive("a", 3);
        let cold = ColdSessions::new(store);
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(cold.directory_reads(), 1);
        assert_eq!(cold.sidecar_directory_reads(), 2);
        cold.store
            .during_tag_read(|| panic!("a refresh read a tag sidecar"));

        for _ in 0..10 {
            assert_eq!(
                labelled(cold.rows()),
                [("a".to_string(), Some("fix-auth".to_string()))],
            );
            assert_eq!(filed(cold.rows()), [("a".to_string(), true)]);
        }
        assert_eq!(cold.directory_reads(), 1, "ten refreshes read no directory");
        assert_eq!(
            cold.sidecar_directory_reads(),
            2,
            "and listed no sidecars either",
        );

        cold.store.during_tag_read(|| {});
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            (cold.directory_reads(), cold.sidecar_directory_reads()),
            (2, 4),
            "an enumeration point reads the store once and lists the sidecar \
             directory once per axis, never once per session",
        );
    }

    /// A sidecar directory the store cannot read costs the labels their
    /// refresh and nothing else. The scan still produces its rows, and the
    /// labels it already holds stand: one unreadable label may not take a
    /// session out of the directory, and it certainly may not blank the
    /// others.
    #[test]
    fn an_unreadable_sidecar_directory_does_not_fail_a_scan() {
        let store = FakeStore::default();
        store.put("a", 5);
        store.put("b", 5);
        store.tag("a", "fix-auth", 6);
        let cold = ColdSessions::new(store);
        cold.enumerate(|_| false).expect("enumerate");

        cold.store.sidecars_unreadable(true);
        cold.store.put("c", 7);
        cold.enumerate(|_| false)
            .expect("a label the scan cannot reach does not fail it");
        assert_eq!(
            labelled(cold.rows()),
            [
                ("a".to_string(), Some("fix-auth".to_string())),
                ("b".to_string(), None),
                ("c".to_string(), None),
            ],
            "the rows are all there and the label we had stands",
        );
        assert_eq!(
            filed(cold.rows()),
            [
                ("a".to_string(), false),
                ("b".to_string(), false),
                ("c".to_string(), false),
            ],
            "and the archived listing failing costs nothing either",
        );
    }

    /// A cold row carries the archived bit the sidecar listing found, and
    /// loses it when the sidecar goes. The listing is the whole answer for
    /// this axis, so it costs no sidecar read at all.
    #[test]
    fn a_cold_row_carries_the_archived_bit_the_listing_found() {
        let store = FakeStore::default();
        store.put("a", 5);
        store.put("b", 5);
        store.archive("a", 6);
        let cold = ColdSessions::new(store);
        cold.store
            .during_tag_read(|| panic!("an archived bit read a tag sidecar"));

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            filed(cold.rows()),
            [("a".to_string(), true), ("b".to_string(), false)],
        );

        cold.store.unarchive("a");
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            filed(cold.rows()),
            [("a".to_string(), false), ("b".to_string(), false)],
            "unarchiving removes the sidecar, and the row follows the store",
        );
    }

    /// A release hands the bit its driver held to the cold row, so a session
    /// archived while it was live is archived the moment it goes cold, with no
    /// enumeration in between. The next scan finds the same sidecar and keeps
    /// the answer.
    #[test]
    fn a_release_hands_the_archived_bit_to_the_cold_row() {
        let store = FakeStore::default();
        store.put("live", 5);
        let cold = ColdSessions::new(store);
        cold.enumerate(|id| id == "live").expect("enumerate");
        assert_eq!(filed(cold.rows()), [], "held live, so no cold row yet");

        // Archived while live: the driver wrote the sidecar under the
        // session's lock and carried the bit in its status.
        cold.store.archive("live", 9);
        cold.note_released(&ReleasedRow {
            archived: true,
            ..released("live", 5, 100)
        });
        assert_eq!(
            filed(cold.rows()),
            [("live".to_string(), true)],
            "the row the release recorded carries the bit",
        );
        assert_eq!(
            cold.directory_reads(),
            1,
            "and it cost no enumeration to learn it",
        );

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            filed(cold.rows()),
            [("live".to_string(), true)],
            "the scan finds the sidecar the release left and agrees",
        );
    }

    /// A scan does not un-archive a session a release archived while it ran,
    /// which is the direction that loses work: the bit would read as cleared
    /// until the next enumeration point, and nothing but the archive command
    /// may clear it.
    ///
    /// The scan's listing predates the sidecar, so only the entry the release
    /// published says the session is archived. Recognising that entry as one
    /// this scan did not look at is what spares it.
    #[test]
    fn a_scan_does_not_unarchive_a_session_a_release_filed() {
        let store = FakeStore::default();
        store.put("held", 5);
        let cold = Arc::new(ColdSessions::new(store));
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(filed(cold.rows()), [("held".to_string(), false)]);

        let releasing = Arc::downgrade(&cold);
        cold.store.during_archived_listing(move || {
            let cold = releasing.upgrade().expect("the cache outlives the scan");
            // Materialized, archived and released while this scan runs: the
            // sidecar is on disk, but the scan listed the directory before it
            // was written.
            cold.store.archive("held", 9);
            cold.note_released(&ReleasedRow {
                archived: true,
                ..released("held", 5, 100)
            });
        });

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            filed(cold.rows()),
            [("held".to_string(), true)],
            "the scan evicted a bit the release published under the lock",
        );
    }

    /// A scan does not re-archive a session a release unarchived while the
    /// scan ran. Its listing is older than the release, which acted under the
    /// session's own lock, and the archived axis reads no file, so the listing
    /// is the only thing the scan knows.
    ///
    /// The rule [`ColdSessions::evict`] states for rows, on the axis where
    /// absence would otherwise be an answer: the release records the `false`
    /// itself, which is what makes it tellable from an id the cache never
    /// held. The scan is deliberately one that believes nothing is live, so
    /// what spares the entry is that comparison and not the live check ahead
    /// of it.
    #[test]
    fn a_scan_does_not_re_archive_a_session_a_release_freed() {
        let store = FakeStore::default();
        store.put("held", 5);
        store.archive("held", 6);
        let cold = Arc::new(ColdSessions::new(store));
        // A first scan, so the cache holds the bit pinned to the sidecar.
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(filed(cold.rows()), [("held".to_string(), true)]);

        let releasing = Arc::downgrade(&cold);
        cold.store.during_archived_listing(move || {
            let cold = releasing.upgrade().expect("the cache outlives the scan");
            // Materialized, unarchived and released while this scan runs: the
            // sidecar is gone and the driver's own answer is `false`, but the
            // scan listed the directory while the file was still there.
            cold.store.unarchive("held");
            cold.note_released(&ReleasedRow {
                archived: false,
                ..released("held", 5, 100)
            });
        });

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            filed(cold.rows()),
            [("held".to_string(), false)],
            "the bit the release handed over outlived the scan's listing",
        );
    }

    /// The sweep's whole job: a rival's hold reaches the row, and the row
    /// follows the rival letting go.
    #[test]
    fn a_cold_row_carries_the_hold_a_sweep_found() {
        let store = FakeStore::default();
        store.put("held", 5);
        store.put("free", 5);
        store.lock("held", true, true);
        store.lock("free", true, false);
        let cold = ColdSessions::new(store);

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            barred(cold.rows()),
            [("free".to_string(), false), ("held".to_string(), true)],
            "a lock a rival holds reads locked, one whose record a crash left does not",
        );

        cold.store.release("held");
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            barred(cold.rows()),
            [("free".to_string(), false), ("held".to_string(), false)],
            "the release cleared the advisory bit",
        );
    }

    /// The record is the filter, not the answer. A settled store is swept
    /// without a single probe, which is what keeps the axis's cost the readdir
    /// and the stats.
    #[test]
    fn a_settled_store_is_swept_without_a_probe() {
        let store = FakeStore::default();
        for id in ["a", "b", "c"] {
            store.put(id, 5);
            // Every session ever minted has a lock file, and a released one
            // carries no record.
            store.lock(id, false, false);
        }
        let cold = ColdSessions::new(store);

        for _ in 0..5 {
            cold.enumerate(|_| false).expect("enumerate");
        }

        assert_eq!(
            barred(cold.rows()),
            [
                ("a".to_string(), false),
                ("b".to_string(), false),
                ("c".to_string(), false)
            ],
        );
        assert_eq!(cold.lock_probes(), 0, "a settled store was probed");
        assert_eq!(
            cold.lock_directory_reads(),
            5,
            "one directory read per enumeration, never one per session",
        );
    }

    /// A holder that failed to write its record holds the lock all the same.
    /// The filter misses it, which is the disclosed cost of the filter, and the
    /// refusal that follows is what corrects the row.
    #[test]
    fn a_hold_with_no_record_reads_free_until_an_attempt_refuses() {
        let store = FakeStore::default();
        store.put("held", 5);
        store.lock("held", false, true);
        let cold = ColdSessions::new(store);

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            barred(cold.rows()),
            [("held".to_string(), false)],
            "an unrecorded holder is not probed, so the sweep cannot see it",
        );
        assert_eq!(cold.lock_probes(), 0);

        cold.note_locked("held", true);
        assert_eq!(
            barred(cold.rows()),
            [("held".to_string(), true)],
            "the attempt is the authority the filter defers to",
        );
    }

    /// A session this host holds is never locked on its own rows, and asking
    /// would say held anyway: `flock` belongs to the open file description, so
    /// this host's own lock refuses this host's probe.
    #[test]
    fn a_live_sessions_lock_is_not_probed() {
        let store = FakeStore::default();
        store.put("mine", 5);
        store.put("theirs", 5);
        // Both locks are held, and only one of them by a rival.
        store.lock("mine", true, true);
        store.lock("theirs", true, true);
        let cold = ColdSessions::new(store);

        cold.enumerate(|id| id == "mine").expect("enumerate");

        assert_eq!(
            cold.store.probes(),
            ["theirs".to_string()],
            "the host probed a lock it holds itself",
        );
        assert!(
            !cold.rows().iter().any(|row| row.id == "mine" && row.locked),
            "the host's own session reads locked on its own row",
        );
    }

    /// A refusal that lands while a sweep is between its listing and its
    /// probes wins. The host asked for the session and was told no, which
    /// outranks a probe taken before the attempt.
    #[test]
    fn a_sweep_does_not_clear_a_refusal_that_landed_while_it_ran() {
        let store = FakeStore::default();
        store.put("held", 5);
        // Free as far as the sweep can tell, so without the guard the sweep
        // writes `false` over the refusal below.
        store.lock("held", false, false);
        let cold = Arc::new(ColdSessions::new(store));
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            barred(cold.rows()),
            [("held".to_string(), false)],
            "the fixture must start unlocked, or the assertion below proves nothing",
        );

        let refusing = Arc::downgrade(&cold);
        cold.store.during_lock_listing(move || {
            let cold = refusing.upgrade().expect("the cache outlives the scan");
            // A rival took it, and this host's own acquire has just been
            // refused, after the listing was taken.
            cold.store.lock("held", true, true);
            cold.note_locked("held", true);
        });

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            barred(cold.rows()),
            [("held".to_string(), true)],
            "the sweep's staler view undid a refusal",
        );
    }

    /// A hold that falls and rises during a sweep returns the bit to its
    /// starting value, but the newer refusal must still win.
    #[test]
    fn a_sweep_does_not_clear_a_hold_that_aba_changed_while_it_ran() {
        let store = FakeStore::default();
        store.put("held", 5);
        store.lock("held", true, true);
        let cold = Arc::new(ColdSessions::new(store));
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(barred(cold.rows()), [("held".to_string(), true)]);

        // This listing is the stale free verdict. After it is captured, the
        // prior hold falls and a host acquire is refused by a new hold.
        cold.store.release("held");
        let changing = Arc::downgrade(&cold);
        cold.store.during_lock_listing(move || {
            let cold = changing.upgrade().expect("the cache outlives the scan");
            assert!(
                cold.note_unlocked("held"),
                "the old hold did not fall, so no ABA occurred",
            );
            cold.store.lock("held", true, true);
            cold.note_locked("held", true);
        });

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            barred(cold.rows()),
            [("held".to_string(), true)],
            "the stale free verdict cleared a new hold whose bit matched the \
             sweep's starting bit",
        );
    }

    /// A scan's live snapshot can predate an acquire. Its probe then sees this
    /// host's own flock, which must not become a rival bit after release.
    #[test]
    fn a_sweep_does_not_publish_a_successful_acquire_as_a_rival() {
        let store = FakeStore::default();
        store.put("mine", 5);
        store.lock("mine", true, false);
        let cold = Arc::new(ColdSessions::new(store));
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(barred(cold.rows()), [("mine".to_string(), false)]);

        let acquiring = Arc::downgrade(&cold);
        cold.store.during_lock_listing(move || {
            let cold = acquiring.upgrade().expect("the cache outlives the scan");
            cold.store.lock("mine", true, true);
            cold.note_locked("mine", false);
        });

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(cold.store.probes(), ["mine", "mine"]);
        assert_eq!(barred(cold.rows()), [("mine".to_string(), false)]);
    }

    /// A lock directory the host cannot read costs the axis its refresh and
    /// nothing else. The bit is a hint, and one that cannot be re-established
    /// must not take a row down with it.
    #[test]
    fn an_unreadable_lock_directory_does_not_fail_a_scan() {
        let store = FakeStore::default();
        store.put("held", 5);
        store.lock("held", true, true);
        let cold = ColdSessions::new(store);
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(barred(cold.rows()), [("held".to_string(), true)]);

        cold.store.locks_unreadable(true);
        cold.store.put("new", 7);
        cold.enumerate(|_| false)
            .expect("a lock directory the scan cannot reach does not fail it");

        assert_eq!(
            barred(cold.rows()),
            [("held".to_string(), true), ("new".to_string(), false)],
            "the rows are all there and the bit we had stands",
        );
    }
}
