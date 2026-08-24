//! The on-disk part of the host's session directory.
//!
//! A `list` frame is produced on a coalescing tick whose frequent trigger is
//! session events, so producing one may not touch the filesystem (spec 6.8). It
//! does not: a refresh reads [`ColdSessions::rows`], which is memory. The rows
//! are brought up to date at enumeration points, which are rare and externally
//! paced (host startup, an explicit session listing, a stream attach), and by
//! the host recording its own structural changes.
//!
//! The host is the single writer of its working directory's store (spec
//! section 5), so this is not a staleness the design has to chase. A
//! concurrent writer's sessions cannot be served by this host anyway, and its
//! activity becomes visible at the next enumeration point. That cuts both ways:
//! a row whose log another process deleted is offered until then, and an attach
//! to it is refused, since membership is answered off the store rather than off
//! these rows.
//!
//! An enumeration reads no log content beyond the format sniff, one first
//! line per file, cached against the `(mtime, size)` it was read at. The only
//! other file it opens is a session's tag sidecar, cached the same way and
//! only for the sessions that have one (spec 6.8). The archived sidecars cost
//! one more listing of the same directory and no read at all: the file's
//! existence is the whole answer. A row itself is built from the `stat` the
//! enumeration already did, which is what keeps host startup off the store's
//! bytes: deriving a cold session's `last_seq` would cost a read of every log
//! in the directory, and the row does not carry one (spec 6.8). One case falls
//! outside the cache, a log the store cannot open is retried at every
//! enumeration, because nothing about the file moves when it becomes readable
//! again. That costs the failed open and nothing more.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex as StdMutex, MutexGuard};

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
    fn enumerate_sessions(&self) -> Result<Vec<SessionMetadata>, ConversationError>;

    /// The fingerprint of one session's log, `Ok(None)` when the store holds
    /// no log under that id. One `stat`, no directory read.
    fn session_metadata(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionMetadata>, ConversationError>;

    /// Whether the log is in the current on-disk format, or `None` when it
    /// could not be read at all. Opens the file and reads its first line.
    fn is_current_format(&self, session_id: &str) -> Option<bool>;

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
    fn enumerate_sessions(&self) -> Result<Vec<SessionMetadata>, ConversationError> {
        ConversationPersistence::enumerate_sessions(self)
    }

    fn session_metadata(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionMetadata>, ConversationError> {
        ConversationPersistence::session_metadata(self, session_id)
    }

    fn is_current_format(&self, session_id: &str) -> Option<bool> {
        ConversationPersistence::is_current_format(self, session_id)
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
/// deriving the position would cost a read of the log (spec 6.8).
#[derive(Clone)]
pub(crate) struct ColdSession {
    pub(crate) id: String,
    pub(crate) last_activity: DateTime<Utc>,
    /// The session's label, `None` when it has no sidecar or none this host
    /// could read (spec 6.8).
    pub(crate) tag: Option<String>,
    /// Whether the user has put the session away, which is the existence of
    /// its archived sidecar.
    pub(crate) archived: bool,
    /// Whether a writer that is not this host holds the session's lock.
    pub(crate) locked: bool,
}

/// The store's sessions as the host last saw them. Both the row and the
/// format verdict behind it are cached against the file they describe, so a
/// scan over a settled store derives nothing.
pub(crate) struct ColdSessions<S> {
    store: S,
    cache: StdMutex<Cache>,
    directory_reads: AtomicU64,
    sidecar_directory_reads: AtomicU64,
    membership_lookups: AtomicU64,
    tag_reads: AtomicU64,
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

/// A log's format verdict, plus the file state it was sniffed at.
struct Sniffed {
    at: Fingerprint,
    current_format: bool,
}

/// A session's label, plus the sidecar state it came from.
///
/// `at` is `None` for a tag the host recorded itself, which a release does
/// with what its driver held (see [`ReleasedRow`]). No fingerprint can match
/// that, so the next enumeration reads the sidecar once and pins the entry to
/// the file from then on.
///
/// `tag` is `None` for a sidecar that reads as no label at all. Caching that
/// is what keeps a hand-mangled sidecar from being re-read at every
/// enumeration: unlike a log the store cannot open, its content is a settled
/// fact about the file.
///
/// The whole value is compared, not just the fingerprint, which is how a scan
/// tells the entry it looked at from one something published while it read a
/// file (see [`ColdSessions::tag`] and [`ColdSessions::evict_tags`]).
#[derive(Clone, PartialEq)]
struct Tagged {
    at: Option<Fingerprint>,
    tag: Option<String>,
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
/// The two sidecar maps are evicted against the sidecar listing rather than
/// the log one, because a sidecar outlives its log: deleting a session's log
/// by hand leaves its label and its archived bit in `meta/`, and the entry
/// stays until the file does.
#[derive(Default)]
struct Cache {
    /// The answer a refresh serves. What an enumeration point last found, plus
    /// what the host has recorded about its own sessions since.
    rows: HashMap<String, Row>,
    formats: HashMap<String, Sniffed>,
    /// One entry per session that has a label. Its absence is the untagged
    /// answer, which is what makes an untagged store cost nothing.
    tags: HashMap<String, Tagged>,
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
    /// No fingerprint, where [`Tagged`] carries one: a fingerprint is what
    /// lets a caller skip re-reading a file, and this axis reads none. The
    /// listing's own report that the sidecar exists is the whole answer.
    archived: HashMap<String, bool>,
    /// The sessions a rival writer holds, as the host last established. Its
    /// members are the rows that read `locked` (spec 6.8).
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
}

impl<S: SessionStore> ColdSessions<S> {
    pub(crate) fn new(store: S) -> Self {
        Self {
            store,
            cache: StdMutex::new(Cache::default()),
            directory_reads: AtomicU64::new(0),
            sidecar_directory_reads: AtomicU64::new(0),
            membership_lookups: AtomicU64::new(0),
            tag_reads: AtomicU64::new(0),
            lock_directory_reads: AtomicU64::new(0),
            lock_probes: AtomicU64::new(0),
        }
    }

    /// The cold rows as they stand, in no particular order.
    ///
    /// Touches no filesystem, which is the whole point: this is what a
    /// refresh serves (spec 6.8).
    pub(crate) fn rows(&self) -> Vec<ColdSession> {
        let cache = self.cache();
        cache
            .rows
            .iter()
            .map(|(id, row)| ColdSession {
                id: id.clone(),
                last_activity: row.last_activity,
                tag: cache.tags.get(id).and_then(|tagged| tagged.tag.clone()),
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
            .and_then(|tagged| tagged.tag.clone())
    }

    /// Whether this host last knew `id` to be archived.
    ///
    /// Touches no filesystem. What a materialization falls back to when it
    /// cannot read the sidecar itself, on the reasoning [`Self::label`] gives.
    pub(crate) fn archived(&self, id: &str) -> bool {
        self.cache().archived.get(id).copied().unwrap_or(false)
    }

    /// Re-read the store and bring the rows up to date. The enumeration point
    /// (spec 6.8), and the only path here that reads the directory.
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
        // taken with their values, which is what makes one published while
        // the scan ran recognisable under an id the scan did see (see
        // [`Self::evict_tags`]). The archived bits are taken the same way and
        // for the same reason (see [`Self::record_archived`]).
        let (known, labelled, filed, held_before) = {
            let cache = self.cache();
            let known: HashSet<String> = cache
                .rows
                .keys()
                .chain(cache.formats.keys())
                .cloned()
                .collect();
            (
                known,
                cache.tags.clone(),
                cache.archived.clone(),
                cache.locked.clone(),
            )
        };
        let enumerated = self.enumerate_store()?;
        for metadata in &enumerated {
            if live(&metadata.session_id) {
                continue;
            }
            // Sniffed outside the guard below: it can open a file.
            let Some(current_format) = self.current_format(metadata) else {
                // The store could not read the log at all, which says nothing
                // about it. Dropping the row over that would take a session
                // out of the directory on a passing EMFILE or permission blip,
                // and a release that had just recorded its row would be the
                // one undone. We keep what we had, and a log that never had a
                // row still has none.
                continue;
            };
            let at = fingerprint(metadata);
            let mut cache = self.cache();
            if !current_format {
                // A pre-refactor log is no session, and one that turns into
                // one stops being listed.
                cache.rows.remove(&metadata.session_id);
                continue;
            }
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
        // reads nothing (spec 6.8).
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
                    self.tag(sidecar);
                }
                self.evict_tags(&sidecars, &labelled);
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
        // another writer (spec 6.8). A stat per lock file, and a probe only of
        // the ones a stat shows a holder record on.
        //
        // A directory this host cannot read costs the axis its refresh and
        // nothing else, on the same reasoning as the tag sidecars: the bit is a
        // hint, and one that cannot be re-established must not take a row down
        // with it.
        match self.enumerate_locks() {
            Ok(locks) => self.record_locked(&self.probe(&locks, &live), &held_before),
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
    /// `held_before` is what the cache held when the sweep started, and the
    /// comparison against it is eligibility rather than truth, exactly as for
    /// the archived bit. An entry that has moved since then was written by a
    /// refused acquire, which is this host having actually asked for the
    /// session and been told no, so it outranks a probe taken before that
    /// attempt and is left alone.
    fn record_locked(&self, verdicts: &[(String, bool)], held_before: &HashSet<String>) {
        let mut cache = self.cache();
        for (session_id, held) in verdicts {
            if cache.locked.contains(session_id) != held_before.contains(session_id) {
                continue;
            }
            if *held {
                cache.locked.insert(session_id.clone());
            } else {
                cache.locked.remove(session_id);
            }
        }
    }

    /// Record that an acquire of `session_id` was refused, or won.
    ///
    /// The refusal source of the bit (spec 6.8), and the authority among the
    /// three: a row says held from the moment this host has been told no, and
    /// stops the moment this host holds the session itself. Returns whether the
    /// bit moved, which is what a caller publishes on.
    pub(crate) fn note_locked(&self, session_id: &str, locked: bool) -> bool {
        let mut cache = self.cache();
        if locked {
            cache.locked.insert(session_id.to_string())
        } else {
            cache.locked.remove(session_id)
        }
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
    /// counter because neither of the other seams can see it: a readdir and a
    /// `stat` transfer no bytes, and [`Self::tag_reads`] counts sidecar
    /// contents, which the archived axis reads none of and this reads none of
    /// either. A refresh that listed the sidecars would be invisible without
    /// it (spec 6.8).
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn sidecar_directory_reads(&self) -> u64 {
        self.sidecar_directory_reads.load(Ordering::Relaxed)
    }

    /// How many tag sidecars this has opened and read.
    ///
    /// The same kind of contract as [`Self::directory_reads`], for the other
    /// per-file read an enumeration is allowed (spec 6.8). A row carries its
    /// label either way, so only this tells a cached answer from a fresh one:
    /// an untagged store must never reach a sidecar, and a settled tagged one
    /// must read each of them exactly once.
    ///
    /// The refresh path's reads, which is where the budget lives. A
    /// materialization reads the sidecar of the one session it is opening,
    /// through the store directly, and that read is not counted here.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn tag_reads(&self) -> u64 {
        self.tag_reads.load(Ordering::Relaxed)
    }

    /// How many membership questions reached the store.
    ///
    /// The other half of the same kind of contract: spec 6.2 wants an id the
    /// grammar rejects turned away *before* a store lookup, and a refusal
    /// leaves no other trace to assert on, since the answer is the same
    /// either way.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn membership_lookups(&self) -> u64 {
        self.membership_lookups.load(Ordering::Relaxed)
    }

    /// How many times this has read the store's lock directory.
    ///
    /// One read per enumeration point, never one per session: the same shape
    /// the sidecar axes are swept with, and the number the spec's cost claim
    /// for the `locked` axis is about (spec 6.8).
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

    /// Whether the store holds a current-format log for `id`.
    ///
    /// The membership test materialization gates on. It costs one `stat` plus
    /// at most one format sniff, so it says nothing about how many sessions
    /// the store holds: that is what the id grammar buys (spec 6.2, and
    /// [`crate::host::validate_session_id`]).
    ///
    /// A log this store cannot stat is a failure rather than an absence, so
    /// a store nothing can read refuses a request loudly instead of reporting
    /// every session in it as gone. A log it can stat but not sniff is still
    /// not a session this host could materialize, which is the one place the
    /// answer folds a read failure into "no".
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
        let Some(metadata) = self.store.session_metadata(id)? else {
            return Ok(false);
        };
        Ok(self.current_format(&metadata).unwrap_or(false))
    }

    /// Record what the host knows about a session it just released, so a
    /// refresh serves it without an enumeration.
    ///
    /// Touches no filesystem. The row carries its own consistency (see
    /// [`ReleasedRow`]): the fingerprint is the file state the release left
    /// behind. It is also the state the next enumeration finds, unless a rival
    /// writer took the freed lock and appended in between, in which case the
    /// fingerprint has moved and the format verdict simply misses.
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
        cache.formats.insert(
            file.session_id.clone(),
            Sniffed {
                at,
                current_format: true,
            },
        );
        // The label the driver held, which is the only current one: it may
        // have been set after the last enumeration read the sidecar. Recorded
        // without a fingerprint, so the next enumeration re-reads the file
        // once and pins the entry to it (see [`Tagged`]).
        match tag {
            Some(tag) => {
                cache.tags.insert(
                    file.session_id.clone(),
                    Tagged {
                        at: None,
                        tag: Some(tag.clone()),
                    },
                );
            }
            None => {
                cache.tags.remove(&file.session_id);
            }
        }
        // The bit the driver held, recorded on the same terms and for the same
        // reason. Unlike the label it is recorded either way: an entry saying
        // the session is not archived is how a release states what it did
        // under the session's lock, which a sidecar listing taken before that
        // write must not undo (see [`Self::record_archived`]).
        cache.archived.insert(file.session_id.clone(), *archived);
    }

    /// The label in `sidecar`, read once per fingerprint into the cache.
    ///
    /// The second per-file read an enumeration is allowed (spec 6.8), and as
    /// with the format sniff the cache is what makes it affordable: a settled
    /// store re-reads no sidecar, and one whose label was just rewritten reads
    /// only that one.
    ///
    /// A sidecar the store cannot read leaves the cache alone rather than
    /// recording "untagged". A read that failed says nothing about the label,
    /// and the alternative would drop a session's tag off its row until the
    /// file changed again.
    fn tag(&self, sidecar: &SidecarMetadata) {
        let at = Fingerprint::of(sidecar.modified_at, sidecar.size_bytes);
        // The entry as it stood before the read, which is the only one this
        // read is an answer about.
        let before = self.cache().tags.get(&sidecar.session_id).cloned();
        if before.as_ref().is_some_and(|tagged| tagged.at == Some(at)) {
            return;
        }
        // Outside the guard: this opens and reads a file, and every other
        // refresh would queue behind it.
        self.tag_reads.fetch_add(1, Ordering::Relaxed);
        let tag = match self.store.read_tag(&sidecar.session_id) {
            Ok(tag) => tag,
            Err(err) => {
                tracing::warn!(
                    session = sidecar.session_id,
                    "could not read a session's tag: {err}"
                );
                return;
            }
        };
        let mut cache = self.cache();
        // A release that landed while the file was being read knows more than
        // this read does: it recorded the label its driver held, under the
        // session's own lock. Overwriting it would pin an older label to the
        // fingerprint this scan saw, where it would stand until the next
        // enumeration point, which is rare and externally paced.
        if cache.tags.get(&sidecar.session_id) != before.as_ref() {
            return;
        }
        cache
            .tags
            .insert(sidecar.session_id.clone(), Tagged { at: Some(at), tag });
    }

    /// The format verdict for `metadata`'s log, sniffed once per fingerprint.
    ///
    /// The one log-content read an enumeration is allowed (spec 6.8), and the
    /// reason it is affordable is this cache: a settled store re-sniffs
    /// nothing.
    ///
    /// Keyed on the fingerprint rather than on the path alone, even though a
    /// log's format never changes: a sniff can land on a file another process
    /// is midway through creating and read a half-written first line. Keying
    /// on the fingerprint retries that once the write lands, while a settled
    /// pre-refactor file, whose fingerprint never moves, is still only read
    /// once.
    ///
    /// `None` when the store could not read the log at all, which is a
    /// different answer from "not the current format" and earns no cache
    /// entry. Its fingerprint does not move when it becomes readable again
    /// (dropping and restoring a read bit leaves size and modification time
    /// alone), so caching that verdict would hide the session from every
    /// client for the life of the host.
    fn current_format(&self, metadata: &SessionMetadata) -> Option<bool> {
        let at = fingerprint(metadata);
        if let Some(cached) = self.sniffed(&metadata.session_id, at) {
            return Some(cached);
        }
        // Outside the guard: the sniff opens and reads a file, and every other
        // refresh would queue behind it.
        let Some(current) = self.store.is_current_format(&metadata.session_id) else {
            tracing::warn!(
                session = metadata.session_id,
                "could not read a session log to place it in the directory"
            );
            return None;
        };
        if !current {
            // Once per fingerprint, so not the per-tick noise an uncached sniff
            // would have produced.
            tracing::info!(
                session = metadata.session_id,
                "leaving a pre-refactor log out of the session directory"
            );
        }
        self.cache().formats.insert(
            metadata.session_id.clone(),
            Sniffed {
                at,
                current_format: current,
            },
        );
        Some(current)
    }

    /// The cached verdict for `id`, if it was sniffed off the file we are
    /// looking at.
    ///
    /// Two concurrent enumerations can hold different generations of one file,
    /// and the loser's insert overwrites the winner's. The cost is one
    /// redundant sniff on the next scan, so no ordering is enforced.
    fn sniffed(&self, id: &str, at: Fingerprint) -> Option<bool> {
        self.cache()
            .formats
            .get(id)
            .filter(|sniffed| sniffed.at == at)
            .map(|sniffed| sniffed.current_format)
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
        cache.formats.retain(|id, _| !gone(id));
    }

    /// Drop the labels whose sidecars are gone, on the rule [`Self::evict`]
    /// states and against the sidecar directory rather than the log one.
    ///
    /// A label leaves with the file it describes, which is not the session's
    /// log: clearing a tag removes only the sidecar, and the session keeps its
    /// row having lost only its label.
    ///
    /// Eligibility is `labelled`, the entries this scan held before it read
    /// anything, values included. An entry that is not there, or that has
    /// since been replaced, was recorded by something that knew more than this
    /// scan's listing did: a newer enumeration, or a release handing over the
    /// label its driver held under the session's own lock. The id alone cannot
    /// tell those apart the way it can for a row, because a label arrives on a
    /// session that already has one.
    fn evict_tags(&self, sidecars: &[SidecarMetadata], labelled: &HashMap<String, Tagged>) {
        let present: HashSet<&str> = sidecars
            .iter()
            .map(|sidecar| sidecar.session_id.as_str())
            .collect();
        let gone = |id: &String, held: &Tagged| {
            labelled.get(id).is_some_and(|before| before == held) && !present.contains(id.as_str())
        };
        self.cache().tags.retain(|id, held| !gone(id, held));
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
        self.store.enumerate_sessions()
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
    use std::sync::Arc;

    use super::*;

    /// A store whose directory the test edits and whose per-file reads it
    /// counts. The counts are the point: the contract is about the reads an
    /// enumeration avoids, which its answers cannot show.
    ///
    /// It can serve only the two per-file reads [`SessionStore`] asks for, a
    /// log's first line and a tag sidecar. That is the contract's strongest
    /// form: producing a directory row cannot reach a log's contents past its
    /// first line.
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
        sniffs: StdMutex<Vec<String>>,
        /// Run once inside the first sniff of a scan, so a test can act while a
        /// scan is between its directory read and its eviction.
        during_sniff: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
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
    /// they are on a real filesystem: a rewrite can move one without the
    /// other, and each on its own has to invalidate the verdict we cached.
    #[derive(Clone)]
    struct FakeFile {
        id: String,
        /// Epoch seconds. Also the row's activity stamp, since that is the
        /// file's modification time.
        modified: i64,
        size: u64,
        /// Whether the format sniff can read the log. A sniff that cannot is
        /// the transient failure the cache must not remember.
        sniffable: bool,
        current_format: bool,
    }

    /// One tag sidecar. Its fingerprint moves independently of the log's, as
    /// it does on disk: relabelling a session touches this file and nothing
    /// else.
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
        /// The size the fingerprint uses, taken from the label so that a
        /// rewrite to a different label of the same length still has to move
        /// the modification time to be noticed, exactly as on disk.
        fn size(&self) -> u64 {
            self.tag.as_ref().map_or(0, |tag| {
                u64::try_from(tag.len()).expect("a label fits a u64")
            })
        }
    }

    impl FakeFile {
        /// A current-format log last written at `modified` (epoch seconds).
        fn current(id: &str, modified: i64) -> Self {
            Self {
                id: id.to_string(),
                modified,
                size: 100,
                sniffable: true,
                current_format: true,
            }
        }
    }

    impl SessionStore for FakeStore {
        fn enumerate_sessions(&self) -> Result<Vec<SessionMetadata>, ConversationError> {
            // `list` preserves the store's order, and the real one is
            // latest-first. Ascending here, so assertions read in order.
            let mut files = self.files.lock().expect("files").clone();
            files.sort_by(|left, right| left.id.cmp(&right.id));
            Ok(files
                .iter()
                .map(|file| SessionMetadata::new(file.id.clone(), at(file.modified), file.size))
                .collect())
        }

        fn is_current_format(&self, session_id: &str) -> Option<bool> {
            self.sniffs
                .lock()
                .expect("sniffs")
                .push(session_id.to_string());
            if let Some(interleave) = self.during_sniff.lock().expect("hook").take() {
                interleave();
            }
            let file = self.file(session_id)?;
            file.sniffable.then_some(file.current_format)
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

        /// Whether `id`'s sidecar can be read, as a permission change on the
        /// file does without moving its fingerprint.
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

        /// Put a current-format log last written at `modified` in the store.
        fn put(&self, id: &str, modified: i64) {
            self.write(FakeFile::current(id, modified));
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

        fn during_sniff(&self, interleave: impl FnOnce() + Send + 'static) {
            *self.during_sniff.lock().expect("hook") = Some(Box::new(interleave));
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

        /// How many times this store sniffed `id`'s first line.
        fn sniffs_of(&self, id: &str) -> usize {
            self.sniffs
                .lock()
                .expect("sniffs")
                .iter()
                .filter(|sniffed| *sniffed == id)
                .count()
        }

        fn forget_sniffs(&self) {
            self.sniffs.lock().expect("sniffs").clear();
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

    /// The archived bits the rows carry, paired with their ids.
    fn barred(cold: Vec<ColdSession>) -> Vec<(String, bool)> {
        let mut rows: Vec<(String, bool)> = cold
            .into_iter()
            .map(|session| (session.id, session.locked))
            .collect();
        rows.sort();
        rows
    }

    fn filed(cold: Vec<ColdSession>) -> Vec<(String, bool)> {
        let mut rows: Vec<(String, bool)> = cold
            .into_iter()
            .map(|session| (session.id, session.archived))
            .collect();
        rows.sort();
        rows
    }

    /// The core of the performance contract: a session the host holds live is
    /// never read from disk, however many refreshes run over it.
    #[test]
    fn a_live_session_is_never_read_from_disk() {
        let store = FakeStore::default();
        store.put("live", 12);
        store.put("cold", 3);
        let cold = ColdSessions::new(store);

        for _ in 0..20 {
            // The live log grows under us, as an append does. Nothing about
            // the refresh may look at it.
            cold.store.edit("live", |file| {
                file.size += 100;
                file.modified += 1;
            });
            assert_eq!(
                refreshed(&cold, |id| id == "live"),
                vec![("cold".to_string(), 3)],
                "the live session is left to the host, the cold one is served",
            );
        }
        assert_eq!(
            cold.store.sniffs_of("live"),
            0,
            "not even a first line is read for a live session",
        );
    }

    /// An enumeration over a settled store reads no file at all: the only
    /// per-file fact a row needs beyond the `stat` is the format verdict, and
    /// that came from the cache.
    ///
    /// This is what makes host startup affordable. A store of hundreds of logs
    /// costs one first-line read each, once, and nothing after that.
    #[test]
    fn an_unchanged_store_is_sniffed_once() {
        let store = FakeStore::default();
        for id in ["a", "b", "c"] {
            store.put(id, 5);
        }
        let cold = ColdSessions::new(store);

        for _ in 0..10 {
            assert_eq!(
                refreshed(&cold, |_| false),
                [
                    ("a".to_string(), 5),
                    ("b".to_string(), 5),
                    ("c".to_string(), 5),
                ],
            );
        }
        for id in ["a", "b", "c"] {
            assert_eq!(
                cold.store.sniffs_of(id),
                1,
                "{id} was sniffed once across ten refreshes",
            );
        }
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

    /// Both halves of the fingerprint invalidate the cached verdict on their
    /// own. Size alone misses a rewrite that preserves the length, and
    /// modification time alone misses two writes inside one clock tick, which
    /// a filesystem with coarse timestamps produces.
    ///
    /// The verdict flips under the test's feet, which no real log does. It is
    /// the only way to observe from the outside whether the sniff ran or the
    /// cache answered, and the last step, where neither half moves and the
    /// stale verdict stands, is what proves the cache is doing the answering.
    #[test]
    fn either_half_of_the_fingerprint_reruns_the_sniff() {
        let store = FakeStore::default();
        store.put("a", 5);
        let cold = ColdSessions::new(store);
        assert_eq!(refreshed(&cold, |_| false), [("a".to_string(), 5)]);

        // Same size, later modification time: a rewrite in place.
        cold.store.forget_sniffs();
        cold.store.edit("a", |file| {
            file.modified += 1;
            file.current_format = false;
        });
        assert_eq!(
            refreshed(&cold, |_| false),
            [],
            "a log rewritten to the same length is sniffed again",
        );
        assert_eq!(cold.store.sniffs_of("a"), 1);

        // Same modification time, larger size: a write inside one tick.
        cold.store.forget_sniffs();
        cold.store.edit("a", |file| {
            file.size += 100;
            file.current_format = true;
        });
        assert_eq!(
            refreshed(&cold, |_| false),
            [("a".to_string(), 6)],
            "a log that grew within one clock tick is sniffed again",
        );
        assert_eq!(cold.store.sniffs_of("a"), 1);

        // And settles once neither half moves: the verdict is answered from
        // the cache, so a flip the fingerprint does not show is not seen.
        cold.store.forget_sniffs();
        cold.store.edit("a", |file| file.current_format = false);
        assert_eq!(refreshed(&cold, |_| false), [("a".to_string(), 6)]);
        assert_eq!(cold.store.sniffs_of("a"), 0);
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
        assert_eq!(
            cold.store.sniffs_of("b"),
            1,
            "and the log that stayed put was not re-read for any of it",
        );
    }

    /// A vanished log takes its row with it, not just its format verdict.
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

    /// A pre-refactor log stays out of the listing, and the verdict costs one
    /// sniff however many enumerations run: the format of a log is a fact
    /// about its content, so it is cacheable.
    #[test]
    fn a_pre_refactor_log_is_left_out() {
        let store = FakeStore::default();
        store.put("current", 3);
        store.write(FakeFile {
            current_format: false,
            ..FakeFile::current("ancient", 4)
        });
        let cold = ColdSessions::new(store);

        for _ in 0..5 {
            assert_eq!(refreshed(&cold, |_| false), [("current".to_string(), 3)]);
        }
        assert_eq!(cold.store.sniffs_of("ancient"), 1, "sniffed once");
    }

    /// A log the store cannot read at all is left out, and that verdict is
    /// *not* cached: nothing about the file moves when it becomes readable
    /// again, so a cached verdict would hide the session for the life of the
    /// host.
    #[test]
    fn an_unsniffable_log_is_retried_at_an_unchanged_fingerprint() {
        let store = FakeStore::default();
        store.write(FakeFile {
            sniffable: false,
            ..FakeFile::current("shy", 6)
        });
        let cold = ColdSessions::new(store);
        for _ in 0..3 {
            assert_eq!(
                refreshed(&cold, |_| false),
                [],
                "a log that cannot be read is no session",
            );
        }
        assert_eq!(
            cold.store.sniffs_of("shy"),
            3,
            "and every enumeration tries it again",
        );

        // Readable again with neither half of the fingerprint moved, which is
        // exactly what restoring a read bit looks like.
        cold.store.edit("shy", |file| file.sniffable = true);
        assert_eq!(
            refreshed(&cold, |_| false),
            [("shy".to_string(), 6)],
            "the session comes back without the file changing",
        );
    }

    /// A log the store cannot read for a moment does not cost the session its
    /// row. The verdict says nothing about the log, so an enumeration that
    /// hits one leaves the directory where it stands rather than dropping a
    /// session out of it, which spec section 5 does not allow a release to be
    /// followed by.
    #[test]
    fn a_transient_read_failure_does_not_drop_a_row() {
        let store = FakeStore::default();
        store.put("a", 3);
        let cold = ColdSessions::new(store);
        cold.note_released(&ReleasedRow {
            file: SessionMetadata::new("a".to_string(), at(3), 100),
            last_activity: at(9),
            tag: None,
            archived: false,
        });
        assert_eq!(listed(cold.rows()), [("a".to_string(), 9)]);

        // Unreadable, at a fingerprint that does not match the release's, so
        // the cached verdict cannot carry the row through either.
        cold.store.edit("a", |file| {
            file.sniffable = false;
            file.modified = 4;
        });
        assert_eq!(
            refreshed(&cold, |_| false),
            [("a".to_string(), 9)],
            "the row the host recorded outlives a failed sniff",
        );

        cold.store.edit("a", |file| file.sniffable = true);
        assert_eq!(
            refreshed(&cold, |_| false),
            [("a".to_string(), 4)],
            "and the file answers again once it can be read",
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
        assert_eq!(cold.store.sniffs_of("held"), 0);
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
            ..FakeFile::current("held", 10)
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

    /// Eviction covers the cached verdict too, not only the rows. A log with
    /// no row still leaves a verdict behind, and one that outlived its file
    /// would answer for whatever file takes the id next.
    #[test]
    fn a_vanished_log_leaves_no_verdict_behind() {
        let store = FakeStore::default();
        store.write(FakeFile {
            current_format: false,
            ..FakeFile::current("a", 5)
        });
        let cold = ColdSessions::new(store);
        assert_eq!(
            refreshed(&cold, |_| false),
            [],
            "a pre-refactor log is no row"
        );

        cold.store.remove("a");
        assert_eq!(refreshed(&cold, |_| false), []);

        // A different file, same id, and a fingerprint the old one also had,
        // which is what a cached verdict would answer from.
        cold.store.put("a", 5);
        assert_eq!(
            refreshed(&cold, |_| false),
            [("a".to_string(), 5)],
            "the verdict outlived the file that produced it",
        );
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
        cold.store.during_sniff(move || {
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

    /// The membership test answers off one `stat` and the cached format
    /// verdict. It never reads the directory, which is what makes it
    /// independent of how many sessions the store holds (spec 6.2).
    #[test]
    fn membership_answers_off_one_stat_and_one_sniff() {
        let store = FakeStore::default();
        store.put("a", 3);
        store.write(FakeFile {
            current_format: false,
            ..FakeFile::current("ancient", 1)
        });
        let cold = ColdSessions::new(store);

        for _ in 0..5 {
            assert!(cold.contains("a").expect("the store answered"));
            assert!(!cold.contains("ancient").expect("the store answered"));
            assert!(!cold.contains("nobody").expect("the store answered"));
        }
        assert_eq!(cold.store.sniffs_of("a"), 1);
        assert_eq!(cold.store.sniffs_of("ancient"), 1);
        assert_eq!(
            cold.directory_reads(),
            0,
            "fifteen membership questions and not one directory read",
        );
    }

    /// A store where nothing is labelled never opens a sidecar, however often
    /// it is enumerated. Untagged is the common case, and an implementation
    /// that asked per session would turn it into a read per row (spec 6.8).
    #[test]
    fn an_untagged_store_reads_no_sidecar() {
        let store = FakeStore::default();
        for id in ["a", "b", "c"] {
            store.put(id, 5);
        }
        let cold = ColdSessions::new(store);

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
        assert_eq!(
            cold.tag_reads(),
            0,
            "five enumerations of an untagged store read a sidecar",
        );
    }

    /// A labelled session's row carries its label, read once and then served
    /// from the cache: a settled store re-reads no sidecar, and rewriting one
    /// label re-reads exactly that one.
    #[test]
    fn a_sidecar_is_read_once_per_fingerprint() {
        let store = FakeStore::default();
        for id in ["a", "b"] {
            store.put(id, 5);
        }
        store.tag("a", "fix-auth", 10);
        store.tag("b", "spike", 10);
        let cold = ColdSessions::new(store);

        for _ in 0..5 {
            cold.enumerate(|_| false).expect("enumerate");
            assert_eq!(
                labelled(cold.rows()),
                [
                    ("a".to_string(), Some("fix-auth".to_string())),
                    ("b".to_string(), Some("spike".to_string())),
                ],
            );
        }
        assert_eq!(
            cold.tag_reads(),
            2,
            "five enumerations over two settled sidecars",
        );

        // One label rewritten, which moves that sidecar and no other.
        cold.store.tag("a", "fix-auth-again", 11);
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            labelled(cold.rows()),
            [
                ("a".to_string(), Some("fix-auth-again".to_string())),
                ("b".to_string(), Some("spike".to_string())),
            ],
        );
        assert_eq!(cold.tag_reads(), 3, "only the sidecar that moved was read");
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

        // And the entry is gone rather than remembered, so a sidecar that
        // comes back at the fingerprint the old one had is read afresh.
        let reads = cold.tag_reads();
        cold.store.tag("a", "fix-auth", 6);
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            labelled(cold.rows()),
            [("a".to_string(), Some("fix-auth".to_string()))],
        );
        assert_eq!(cold.tag_reads(), reads + 1);
    }

    /// A sidecar that says nothing usable reads as no label, and that verdict
    /// is cached: its content is a settled fact about the file, unlike a log
    /// the store could not open at all.
    #[test]
    fn an_unusable_sidecar_reads_as_untagged_once() {
        let store = FakeStore::default();
        store.put("a", 5);
        store.write_sidecar(FakeSidecar {
            id: "a".to_string(),
            modified: 6,
            tag: None,
            readable: true,
        });
        let cold = ColdSessions::new(store);

        for _ in 0..3 {
            cold.enumerate(|_| false).expect("enumerate");
            assert_eq!(labelled(cold.rows()), [("a".to_string(), None)]);
        }
        assert_eq!(cold.tag_reads(), 1, "the empty answer was cached too");
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

        for _ in 0..3 {
            cold.enumerate(|id| id == "live").expect("enumerate");
        }
        assert_eq!(cold.tag_reads(), 0);

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
        assert_eq!(cold.tag_reads(), 0, "the release read nothing");

        // The next enumeration pins the entry to the file, which costs the
        // one read a released label has not had.
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            labelled(cold.rows()),
            [("live".to_string(), Some("on disk".to_string()))],
        );
        assert_eq!(cold.tag_reads(), 1);
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(cold.tag_reads(), 1, "and it settles there");
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

    /// A release that lands while a scan is reading a sidecar outranks what
    /// that read returns. The release held the session's own lock and the
    /// label its driver had, so the scan's answer is the older one, and
    /// writing it back would pin it until the next enumeration point.
    #[test]
    fn a_release_during_a_sidecar_read_outranks_what_the_scan_read() {
        let store = FakeStore::default();
        store.put("a", 5);
        store.tag("a", "old", 6);
        let cold = Arc::new(ColdSessions::new(store));

        let releasing = Arc::downgrade(&cold);
        cold.store.during_tag_read(move || {
            let cold = releasing.upgrade().expect("the cache outlives the scan");
            // Materialized, relabelled and released while the scan's read of
            // the sidecar is in flight.
            cold.store.tag("a", "new", 7);
            cold.note_released(&ReleasedRow {
                tag: Some("new".to_string()),
                ..released("a", 5, 100)
            });
        });

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            labelled(cold.rows()),
            [("a".to_string(), Some("new".to_string()))],
            "the scan kept the label the release published",
        );

        // And the next scan pins the entry to the file, which by then holds
        // the same label.
        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            labelled(cold.rows()),
            [("a".to_string(), Some("new".to_string()))],
        );
    }

    /// A sidecar the store cannot read leaves the cache alone rather than
    /// recording "untagged". The read says nothing about the label, and its
    /// fingerprint does not move when the file becomes readable again, so a
    /// cached "untagged" would stand for the life of the host.
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

        // Unreadable at a moved fingerprint, so the cached entry cannot
        // answer and the read is the only thing that could.
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

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            filed(cold.rows()),
            [("a".to_string(), true), ("b".to_string(), false)],
        );
        assert_eq!(
            cold.tag_reads(),
            0,
            "the bit is the file's existence, so nothing was opened to learn it",
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
            "the release cleared the bit, which is the edge a refused client waits on",
        );
    }

    /// The record is the filter, not the answer. A settled store is swept
    /// without a single probe, which is what keeps the axis's cost the readdir
    /// and the stats (spec 6.8).
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

        assert!(
            cold.note_locked("held", true),
            "the refusal moved the bit, so the caller has a frame to publish",
        );
        assert_eq!(
            barred(cold.rows()),
            [("held".to_string(), true)],
            "the attempt is the authority the filter defers to",
        );
        assert!(
            !cold.note_locked("held", true),
            "a second refusal of the same session moves nothing",
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
