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
//! line per file, cached against the `(mtime, size)` it was read at. A row
//! itself is built from the `stat` the enumeration already did, which is what
//! keeps host startup off the store's bytes: deriving a cold session's
//! `last_seq` would cost a read of every log in the directory, and the row
//! does not carry one (spec 6.8). One case falls outside the cache, a log the
//! store cannot open is retried at every enumeration, because nothing about
//! the file moves when it becomes readable again. That costs the failed open
//! and nothing more.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex as StdMutex, MutexGuard};

use aj_session::{ConversationError, ConversationPersistence, SessionMetadata};

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
}

/// One session the store holds that the host is not holding live.
///
/// No durable position: a cold row carries an activity stamp instead, and
/// deriving the position would cost a read of the log (spec 6.8).
#[derive(Clone)]
pub(crate) struct ColdSession {
    pub(crate) id: String,
    pub(crate) last_activity: DateTime<Utc>,
}

/// The store's sessions as the host last saw them. Both the row and the
/// format verdict behind it are cached against the file they describe, so a
/// scan over a settled store derives nothing.
pub(crate) struct ColdSessions<S> {
    store: S,
    cache: StdMutex<Cache>,
    directory_reads: AtomicU64,
    membership_lookups: AtomicU64,
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

/// A log's format verdict, plus the file state it was sniffed at.
struct Sniffed {
    at: Fingerprint,
    current_format: bool,
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

/// Both maps are keyed by session id. [`ColdSessions::enumerate`] drops the
/// entries of sessions that have left the store, so on a host that ever
/// enumerates (every host does, at startup) neither outgrows it. Entries
/// that [`ColdSessions::contains`] adds in between are not evicted until the
/// next enumeration.
#[derive(Default)]
struct Cache {
    /// The answer a refresh serves. What an enumeration point last found, plus
    /// what the host has recorded about its own sessions since.
    rows: HashMap<String, Row>,
    formats: HashMap<String, Sniffed>,
}

impl<S: SessionStore> ColdSessions<S> {
    pub(crate) fn new(store: S) -> Self {
        Self {
            store,
            cache: StdMutex::new(Cache::default()),
            directory_reads: AtomicU64::new(0),
            membership_lookups: AtomicU64::new(0),
        }
    }

    /// The cold rows as they stand, in no particular order.
    ///
    /// Touches no filesystem, which is the whole point: this is what a
    /// refresh serves (spec 6.8).
    pub(crate) fn rows(&self) -> Vec<ColdSession> {
        self.cache()
            .rows
            .iter()
            .map(|(id, row)| ColdSession {
                id: id.clone(),
                last_activity: row.last_activity,
            })
            .collect()
    }

    /// The activity stamp this host holds for `id`, if it holds a row for it.
    ///
    /// Touches no filesystem.
    pub(crate) fn stamp(&self, id: &str) -> Option<DateTime<Utc>> {
        self.cache().rows.get(id).map(|row| row.last_activity)
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
        let known: HashSet<String> = {
            let cache = self.cache();
            cache
                .rows
                .keys()
                .chain(cache.formats.keys())
                .cloned()
                .collect()
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
        self.evict(&enumerated, &known);
        Ok(())
    }

    /// How many times this has read the store's directory.
    ///
    /// The refresh contract is about the filesystem work a refresh does *not*
    /// do, which its answers cannot show, so this is the seam the tests assert
    /// on. Only [`Self::enumerate`] reads the directory: a membership question
    /// is answered off a single `stat` (see [`Self::contains`]).
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn directory_reads(&self) -> u64 {
        self.directory_reads.load(Ordering::Relaxed)
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

    fn enumerate_store(&self) -> Result<Vec<SessionMetadata>, ConversationError> {
        self.directory_reads.fetch_add(1, Ordering::Relaxed);
        self.store.enumerate_sessions()
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().expect("cold session cache poisoned")
    }
}

fn fingerprint(metadata: &SessionMetadata) -> Fingerprint {
    Fingerprint {
        modified: metadata.modified_at,
        size: metadata.size_bytes,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// A store whose directory the test edits and whose per-file reads it
    /// counts. The counts are the point: the contract is about the reads an
    /// enumeration avoids, which its answers cannot show.
    ///
    /// It can only serve one kind of read, because [`SessionStore`] only asks
    /// for one. That is the contract's strongest form: producing a directory
    /// row cannot reach a log's contents past its first line.
    #[derive(Default)]
    struct FakeStore {
        files: StdMutex<Vec<FakeFile>>,
        sniffs: StdMutex<Vec<String>>,
        /// Run once inside the first sniff of a scan, so a test can act while a
        /// scan is between its directory read and its eviction.
        during_sniff: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
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
        }
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
            cold.note_released(&released("late", 7, 400));
        });

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            listed(cold.rows()),
            [("a".to_string(), 2), ("late".to_string(), 7)],
            "the scan evicted a row it never had a view of",
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
}
