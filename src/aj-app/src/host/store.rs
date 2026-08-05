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
//! An enumeration itself does per-file work only where a file moved: the
//! format verdict and the entry count cache against the `(mtime, size)` they
//! were read from. One case falls outside that, a log the store cannot open is
//! retried at every enumeration, because nothing about the file moves when it
//! becomes readable again. That costs the failed open and nothing more.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex as StdMutex, MutexGuard};

use aj_session::{ConversationError, ConversationPersistence, SessionMetadata};

use crate::host::live::ReleasedMark;
use chrono::{DateTime, Utc};

/// What a directory refresh needs from the session store.
///
/// Behind a trait because what this module exists for is the reads it does
/// *not* perform, which the values it returns cannot show. The tests drive it
/// with a store that counts them.
pub(crate) trait SessionStore {
    /// Every session log in the store, with its fingerprint. Opens no file.
    fn enumerate_sessions(&self) -> Result<Vec<SessionMetadata>, ConversationError>;

    /// Whether the log is in the current on-disk format, or `None` when it
    /// could not be read at all. Opens the file and reads its first line.
    fn is_current_format(&self, session_id: &str) -> Option<bool>;

    /// The durable high-water mark the log records. Reads the whole file.
    fn stored_last_seq(&self, session_id: &str) -> Result<u64, ConversationError>;
}

impl SessionStore for ConversationPersistence {
    fn enumerate_sessions(&self) -> Result<Vec<SessionMetadata>, ConversationError> {
        ConversationPersistence::enumerate_sessions(self)
    }

    fn is_current_format(&self, session_id: &str) -> Option<bool> {
        ConversationPersistence::is_current_format(self, session_id)
    }

    fn stored_last_seq(&self, session_id: &str) -> Result<u64, ConversationError> {
        ConversationPersistence::stored_last_seq(self, session_id)
    }
}

/// One session the store holds that the host is not holding live.
#[derive(Clone)]
pub(crate) struct ColdSession {
    pub(crate) id: String,
    pub(crate) last_seq: u64,
    pub(crate) last_activity: DateTime<Utc>,
}

/// The store's sessions as the host last saw them, with the per-file facts a
/// directory entry needs cached against the file they were read from.
pub(crate) struct ColdSessions<S> {
    store: S,
    cache: StdMutex<Cache>,
    directory_reads: AtomicU64,
}

/// A log file's identity for caching: a file whose modification time and size
/// have not moved cannot have changed shape or grown an entry.
///
/// Not a content hash, so a rewrite that preserves both is invisible to it.
/// Only a hand-edited log does that, and the alternative is reading every log
/// on every tick.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    modified: DateTime<Utc>,
    size: u64,
}

/// A fact derived from a log, plus the file state it was derived from.
struct Derived<T> {
    at: Fingerprint,
    value: T,
}

/// Every map is keyed by session id. [`ColdSessions::enumerate`] drops the
/// entries of sessions that have left the store, so on a host that ever
/// enumerates (every host does, at startup) none of them outgrows it. Entries
/// that [`ColdSessions::contains`] adds in between are not evicted until the
/// next enumeration.
#[derive(Default)]
struct Cache {
    /// The answer a refresh serves. What an enumeration point last found, plus
    /// what the host has recorded about its own sessions since.
    rows: HashMap<String, ColdSession>,
    formats: HashMap<String, Derived<bool>>,
    last_seqs: HashMap<String, Derived<u64>>,
}

impl<S: SessionStore> ColdSessions<S> {
    pub(crate) fn new(store: S) -> Self {
        Self {
            store,
            cache: StdMutex::new(Cache::default()),
            directory_reads: AtomicU64::new(0),
        }
    }

    /// The cold rows as they stand, in no particular order.
    ///
    /// Touches no filesystem, which is the whole point: this is what a
    /// refresh serves (spec 6.8).
    pub(crate) fn rows(&self) -> Vec<ColdSession> {
        self.cache().rows.values().cloned().collect()
    }

    /// Re-read the store and bring the rows up to date. The enumeration point
    /// (spec 6.8), and the only path here that reads the directory.
    ///
    /// `live` names the sessions the host holds. Their logs are enumerated like
    /// any other, but nothing is derived from them: the host answers a live
    /// session off its own status, and recounting a log that is mid-append
    /// would be wrong as well as expensive. Their rows are left as they stand
    /// rather than dropped, so a session released while this runs keeps the row
    /// its release recorded instead of falling out of the directory until the
    /// next enumeration point.
    pub(crate) fn enumerate(&self, live: impl Fn(&str) -> bool) -> Result<(), ConversationError> {
        // What this scan is entitled to evict, taken before the directory read
        // so that everything in it predates this scan's view of the store.
        let known: HashSet<String> = {
            let cache = self.cache();
            cache
                .rows
                .keys()
                .chain(cache.formats.keys())
                .chain(cache.last_seqs.keys())
                .cloned()
                .collect()
        };
        let enumerated = self.enumerate_store()?;
        for metadata in &enumerated {
            if live(&metadata.session_id) {
                continue;
            }
            let row = self.current_format(metadata).then(|| ColdSession {
                id: metadata.session_id.clone(),
                last_seq: self.last_seq(metadata),
                last_activity: metadata.modified_at,
            });
            let mut cache = self.cache();
            match row {
                Some(row) => cache.rows.insert(metadata.session_id.clone(), row),
                // A log the store will not read is no session, and one that
                // stops being readable stops being listed.
                None => cache.rows.remove(&metadata.session_id),
            };
        }
        self.evict(&enumerated, &known);
        Ok(())
    }

    /// How many times this has read the store's directory, whether to enumerate
    /// or to answer a membership question.
    ///
    /// The refresh contract is about the filesystem work a refresh does *not*
    /// do, which its answers cannot show, so this is the seam the tests assert
    /// on.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn directory_reads(&self) -> u64 {
        self.directory_reads.load(Ordering::Relaxed)
    }

    /// Whether the store holds a current-format log for `id`.
    ///
    /// The membership test materialization gates on. It never counts a log's
    /// entries, so asking it about a store full of cold sessions does not read
    /// them.
    pub(crate) fn contains(&self, id: &str) -> Result<bool, ConversationError> {
        let Some(metadata) = self
            .enumerate_store()?
            .into_iter()
            .find(|metadata| metadata.session_id == id)
        else {
            return Ok(false);
        };
        Ok(self.current_format(&metadata))
    }

    /// Record what the host knows about a session it just released, so a
    /// refresh serves it without an enumeration and without counting a log the
    /// host itself closed.
    ///
    /// Touches no filesystem. The mark carries its own consistency (see
    /// [`ReleasedMark`]): the fingerprint is the file state its `last_seq` was
    /// counted at. It is also the state the next enumeration finds, unless a
    /// rival writer took the freed lock and appended in between, in which case
    /// the fingerprint has moved and the derived facts simply miss.
    pub(crate) fn note_released(&self, mark: &ReleasedMark) {
        let ReleasedMark { last_seq, file } = mark;
        let at = fingerprint(file);
        let mut cache = self.cache();
        cache.rows.insert(
            file.session_id.clone(),
            ColdSession {
                id: file.session_id.clone(),
                last_seq: *last_seq,
                last_activity: file.modified_at,
            },
        );
        cache
            .formats
            .insert(file.session_id.clone(), Derived { at, value: true });
        cache.last_seqs.insert(
            file.session_id.clone(),
            Derived {
                at,
                value: *last_seq,
            },
        );
    }

    /// The format verdict for `metadata`'s log, sniffed once per fingerprint.
    ///
    /// Keyed on the fingerprint rather than on the path alone, even though a
    /// log's format never changes: a sniff can land on a file another process
    /// is midway through creating and read a half-written first line. Keying
    /// on the fingerprint retries that once the write lands, while a settled
    /// pre-refactor file, whose fingerprint never moves, is still only read
    /// once.
    ///
    /// A log the store could not read at all earns no cache entry. Its
    /// fingerprint does not move when it becomes readable again (dropping and
    /// restoring a read bit leaves size and modification time alone), so
    /// caching that verdict would hide the session from every client for the
    /// life of the host.
    fn current_format(&self, metadata: &SessionMetadata) -> bool {
        let at = fingerprint(metadata);
        if let Some(cached) = hit(&self.cache().formats, &metadata.session_id, at) {
            return cached;
        }
        // Outside the guard: the sniff opens and reads a file, and every other
        // refresh would queue behind it.
        let Some(current) = self.store.is_current_format(&metadata.session_id) else {
            tracing::warn!(
                session = metadata.session_id,
                "leaving an unreadable log out of the session directory"
            );
            return false;
        };
        if !current {
            // Once per fingerprint, so not the per-tick noise an uncached sniff
            // would have produced.
            tracing::info!(
                session = metadata.session_id,
                "leaving a pre-refactor log out of the session directory"
            );
        }
        self.cache()
            .formats
            .insert(metadata.session_id.clone(), Derived { at, value: current });
        current
    }

    /// The durable high-water mark of `metadata`'s log, counted once per
    /// fingerprint.
    ///
    /// Derived rather than reported as zero, because the unseen-output glyph a
    /// client derives (spec 6.8) is about exactly the sessions it has not
    /// attached, which is most of them.
    ///
    /// A log that cannot be read counts zero and is not cached: a directory
    /// listing must not fail over one unreadable file, and the next refresh
    /// tries again.
    fn last_seq(&self, metadata: &SessionMetadata) -> u64 {
        let at = fingerprint(metadata);
        if let Some(cached) = hit(&self.cache().last_seqs, &metadata.session_id, at) {
            return cached;
        }
        let last_seq = match self.store.stored_last_seq(&metadata.session_id) {
            Ok(last_seq) => last_seq,
            Err(err) => {
                tracing::warn!(
                    session = metadata.session_id,
                    "could not count the log's entries: {err}"
                );
                return 0;
            }
        };
        self.cache().last_seqs.insert(
            metadata.session_id.clone(),
            Derived {
                at,
                value: last_seq,
            },
        );
        last_seq
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
        cache.last_seqs.retain(|id, _| !gone(id));
    }

    fn enumerate_store(&self) -> Result<Vec<SessionMetadata>, ConversationError> {
        self.directory_reads.fetch_add(1, Ordering::Relaxed);
        self.store.enumerate_sessions()
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().expect("cold session cache poisoned")
    }
}

/// A cached value, if it was derived from the file we are looking at.
///
/// Two concurrent refreshes can hold different generations of one file, and
/// the loser's insert overwrites the winner's. The cost is one redundant read
/// on the next tick, so no ordering is enforced.
fn hit<T: Copy>(map: &HashMap<String, Derived<T>>, id: &str, at: Fingerprint) -> Option<T> {
    map.get(id)
        .filter(|derived| derived.at == at)
        .map(|derived| derived.value)
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
    /// counts. The counts are the point: the contract is about the reads a
    /// refresh avoids, which its answers cannot show.
    #[derive(Default)]
    struct FakeStore {
        files: StdMutex<Vec<FakeFile>>,
        reads: StdMutex<Vec<Read>>,
        /// Run once inside the first sniff of a scan, so a test can act while a
        /// scan is between its directory read and its eviction.
        during_sniff: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    /// One log in the fake store. `modified` and `size` are independent, as
    /// they are on a real filesystem: a rewrite can move one without the
    /// other, and each on its own has to invalidate what we derived.
    #[derive(Clone)]
    struct FakeFile {
        id: String,
        modified: i64,
        size: u64,
        entries: u64,
        /// Whether the format sniff can read the log. A sniff that cannot is
        /// the transient failure the cache must not remember.
        sniffable: bool,
        /// Whether counting the log's entries succeeds. Separate from
        /// `sniffable`, because the count reads the whole file and can fail
        /// where the first line read did not.
        countable: bool,
        current_format: bool,
    }

    impl FakeFile {
        /// A current-format log of `entries` entries, both fingerprint
        /// components derived from the count.
        fn current(id: &str, entries: u64) -> Self {
            Self {
                id: id.to_string(),
                modified: i64::try_from(entries).expect("a count"),
                size: entries * 100,
                entries,
                sniffable: true,
                countable: true,
                current_format: true,
            }
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Read {
        Format(String),
        Count(String),
    }

    impl SessionStore for FakeStore {
        fn enumerate_sessions(&self) -> Result<Vec<SessionMetadata>, ConversationError> {
            // `list` preserves the store's order, and the real one is
            // latest-first. Ascending here, so assertions read in order.
            let mut files = self.files.lock().expect("files").clone();
            files.sort_by(|left, right| left.id.cmp(&right.id));
            Ok(files
                .iter()
                .map(|file| {
                    SessionMetadata::new(
                        file.id.clone(),
                        DateTime::UNIX_EPOCH + chrono::Duration::seconds(file.modified),
                        file.size,
                    )
                })
                .collect())
        }

        fn is_current_format(&self, session_id: &str) -> Option<bool> {
            self.reads
                .lock()
                .expect("reads")
                .push(Read::Format(session_id.to_string()));
            if let Some(interleave) = self.during_sniff.lock().expect("hook").take() {
                interleave();
            }
            let file = self.file(session_id)?;
            file.sniffable.then_some(file.current_format)
        }

        fn stored_last_seq(&self, session_id: &str) -> Result<u64, ConversationError> {
            self.reads
                .lock()
                .expect("reads")
                .push(Read::Count(session_id.to_string()));
            // A log that vanished between the enumeration and this read counts
            // zero, the way the real store reports a missing file.
            let Some(file) = self.file(session_id) else {
                return Ok(0);
            };
            if !file.countable {
                return Err(ConversationError::Corrupt("mid-file read".to_string()));
            }
            Ok(file.entries)
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

        /// Put a current-format log of `entries` entries in the store.
        fn put(&self, id: &str, entries: u64) {
            self.write(FakeFile::current(id, entries));
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

        /// The `(sniffs, counts)` this store served for `id`.
        fn reads_of(&self, id: &str) -> (usize, usize) {
            let reads = self.reads.lock().expect("reads");
            (
                reads
                    .iter()
                    .filter(|read| *read == &Read::Format(id.to_string()))
                    .count(),
                reads
                    .iter()
                    .filter(|read| *read == &Read::Count(id.to_string()))
                    .count(),
            )
        }

        fn forget_reads(&self) {
            self.reads.lock().expect("reads").clear();
        }
    }

    /// The listed ids paired with their marks, for compact assertions.
    fn listed(cold: Vec<ColdSession>) -> Vec<(String, u64)> {
        let mut rows: Vec<(String, u64)> = cold
            .into_iter()
            .map(|session| (session.id, session.last_seq))
            .collect();
        rows.sort();
        rows
    }

    /// An enumeration point followed by a refresh, which is the pair every
    /// assertion about what the store holds goes through.
    fn refreshed(
        cold: &ColdSessions<FakeStore>,
        live: impl Fn(&str) -> bool,
    ) -> Vec<(String, u64)> {
        cold.enumerate(live).expect("enumerate");
        listed(cold.rows())
    }

    /// A released session's mark, as its driver would hand it over.
    fn mark(id: &str, last_seq: u64, size: u64) -> ReleasedMark {
        ReleasedMark {
            last_seq,
            file: SessionMetadata::new(id.to_string(), Utc::now(), size),
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

        for appended in 0..20 {
            // The live log grows under us, as an append does. Nothing about
            // the refresh may look at it.
            cold.store.edit("live", |file| {
                file.size += 100;
                file.modified += 1;
                file.entries += appended;
            });
            assert_eq!(
                refreshed(&cold, |id| id == "live"),
                vec![("cold".to_string(), 3)],
                "the live session is left to the host, the cold one is served",
            );
        }
        assert_eq!(
            cold.store.reads_of("live"),
            (0, 0),
            "no sniff and no count for a live session",
        );
    }

    /// A refresh that finds nothing changed on disk reads no file: every
    /// per-file fact came from the cache.
    #[test]
    fn an_unchanged_store_is_read_once() {
        let store = FakeStore::default();
        for id in ["a", "b", "c"] {
            store.put(id, 5);
        }
        let cold = ColdSessions::new(store);

        for _ in 0..10 {
            assert_eq!(refreshed(&cold, |_| false).len(), 3);
        }
        for id in ["a", "b", "c"] {
            assert_eq!(
                cold.store.reads_of(id),
                (1, 1),
                "{id} was sniffed and counted once across ten refreshes",
            );
        }
    }

    /// Both halves of the fingerprint invalidate on their own. Size alone
    /// misses a rewrite that preserves the length, and modification time alone
    /// misses two appends inside one clock tick, which a filesystem with
    /// coarse timestamps produces.
    #[test]
    fn either_half_of_the_fingerprint_invalidates() {
        let store = FakeStore::default();
        store.put("a", 5);
        let cold = ColdSessions::new(store);
        assert_eq!(refreshed(&cold, |_| false), [("a".to_string(), 5)]);

        // Same size, later modification time: a rewrite in place.
        cold.store.forget_reads();
        cold.store.edit("a", |file| {
            file.modified += 1;
            file.entries = 8;
        });
        assert_eq!(
            refreshed(&cold, |_| false),
            [("a".to_string(), 8)],
            "a log rewritten to the same length is read again",
        );
        assert_eq!(cold.store.reads_of("a"), (1, 1));

        // Same modification time, larger size: an append inside one tick.
        cold.store.forget_reads();
        cold.store.edit("a", |file| {
            file.size += 100;
            file.entries = 9;
        });
        assert_eq!(
            refreshed(&cold, |_| false),
            [("a".to_string(), 9)],
            "a log that grew within one clock tick is read again",
        );
        assert_eq!(cold.store.reads_of("a"), (1, 1));

        // And settles again once neither half moves.
        cold.store.forget_reads();
        assert_eq!(refreshed(&cold, |_| false), [("a".to_string(), 9)]);
        assert_eq!(cold.store.reads_of("a"), (0, 0));
    }

    /// A log that appears is picked up, one that vanishes disappears, and its
    /// cache entry goes with it: an id that comes back is derived afresh
    /// rather than answered from what the old file said.
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

        // Same id, same fingerprint, different file. The eviction is what
        // keeps the stale answer from surviving the file that produced it.
        cold.store.forget_reads();
        cold.store.write(FakeFile {
            entries: 7,
            ..FakeFile::current("a", 2)
        });
        assert_eq!(
            refreshed(&cold, |_| false),
            [("a".to_string(), 7), ("b".to_string(), 4)],
        );
        assert_eq!(cold.store.reads_of("a"), (1, 1));
        assert_eq!(cold.store.reads_of("b"), (0, 0));
    }

    /// A pre-refactor log stays out of the listing, and the verdict costs one
    /// sniff however many refreshes run: the format of a log is a fact about
    /// its content, so it is cacheable. Its entries are never counted, since
    /// the listing has no row to put the count on.
    #[test]
    fn a_pre_refactor_log_is_left_out_and_never_counted() {
        let store = FakeStore::default();
        store.put("current", 3);
        store.write(FakeFile {
            current_format: false,
            ..FakeFile::current("ancient", 4)
        });
        let cold = ColdSessions::new(store);

        for _ in 0..5 {
            assert_eq!(refreshed(&cold, |_| false), [("current".to_string(), 3)],);
        }
        assert_eq!(
            cold.store.reads_of("ancient"),
            (1, 0),
            "sniffed once, never counted",
        );
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
            cold.store.reads_of("shy"),
            (3, 0),
            "and every refresh tries it again",
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

    /// A log whose entries cannot be counted is listed with a zero mark rather
    /// than failing the listing, and the failure is not cached either: the
    /// mark is what the unseen-output glyph reads, so a wrong zero must not
    /// outlive the read that produced it.
    #[test]
    fn an_uncountable_log_reports_zero_and_is_retried() {
        let store = FakeStore::default();
        store.write(FakeFile {
            countable: false,
            ..FakeFile::current("torn", 6)
        });
        let cold = ColdSessions::new(store);
        for _ in 0..3 {
            assert_eq!(
                refreshed(&cold, |_| false),
                [("torn".to_string(), 0)],
                "it is still a session, with nothing durable to report",
            );
        }
        assert_eq!(
            cold.store.reads_of("torn"),
            (1, 3),
            "the format verdict held, the count was retried",
        );

        cold.store.edit("torn", |file| file.countable = true);
        assert_eq!(refreshed(&cold, |_| false), [("torn".to_string(), 6)],);
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
    /// costs neither an enumeration nor a count of the log it just closed.
    #[test]
    fn a_released_session_is_served_without_an_enumeration() {
        let store = FakeStore::default();
        store.put("held", 4);
        let cold = ColdSessions::new(store);
        // Held live, so the enumeration leaves it out.
        assert_eq!(refreshed(&cold, |id| id == "held"), []);

        cold.note_released(&mark("held", 4, 400));
        assert_eq!(listed(cold.rows()), [("held".to_string(), 4)]);
        assert_eq!(cold.directory_reads(), 1, "the release read nothing");
        assert_eq!(cold.store.reads_of("held"), (0, 0));
    }

    /// A live session's row is left alone by an enumeration rather than
    /// dropped. The host's live snapshot can predate a release, and the row the
    /// release recorded must not be undone by a scan that still believes the
    /// session is held.
    #[test]
    fn an_enumeration_leaves_a_live_sessions_row_alone() {
        let store = FakeStore::default();
        store.put("a", 2);
        let cold = ColdSessions::new(store);
        assert_eq!(refreshed(&cold, |_| false), [("a".to_string(), 2)]);

        // Materialized, appended to, and released, all while a scan that
        // snapshotted the live set beforehand is still running.
        cold.store.edit("a", |file| {
            file.entries = 5;
            file.size += 300;
        });
        cold.note_released(&mark("a", 5, 800));
        assert_eq!(refreshed(&cold, |id| id == "a"), [("a".to_string(), 5)]);
        assert_eq!(
            cold.store.reads_of("a"),
            (1, 1),
            "and the log was not re-read to produce it",
        );
    }

    /// Eviction covers the derived facts too, not only the rows. A log with no
    /// row still leaves a cached format verdict behind, and a verdict that
    /// outlived its file would answer for whatever file takes the id next.
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
        cold.store.write(FakeFile {
            entries: 7,
            ..FakeFile::current("a", 5)
        });
        assert_eq!(
            refreshed(&cold, |_| false),
            [("a".to_string(), 7)],
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
            cold.note_released(&mark("late", 7, 400));
        });

        cold.enumerate(|_| false).expect("enumerate");
        assert_eq!(
            listed(cold.rows()),
            [("a".to_string(), 2), ("late".to_string(), 7)],
            "the scan evicted a row it never had a view of",
        );
    }

    /// The membership test answers off the enumeration and the cached format
    /// verdict, and never counts entries.
    #[test]
    fn membership_answers_without_counting() {
        let store = FakeStore::default();
        store.put("a", 3);
        store.write(FakeFile {
            current_format: false,
            ..FakeFile::current("ancient", 1)
        });
        let cold = ColdSessions::new(store);

        for _ in 0..5 {
            assert!(cold.contains("a").expect("contains"));
            assert!(!cold.contains("ancient").expect("contains"));
            assert!(!cold.contains("nobody").expect("contains"));
        }
        assert_eq!(cold.store.reads_of("a"), (1, 0));
        assert_eq!(cold.store.reads_of("ancient"), (1, 0));
    }
}
