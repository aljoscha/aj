//! Advisory single-writer lock for one session.
//!
//! Materializing a session in a host process takes this lock and holds it
//! for the session's live lifetime, so a second process that tries the
//! same session refuses rather than growing a sibling branch in a shared
//! log (spec section 5).
//!
//! The lock file doubles as the registry of minted session ids: creating
//! a session claims its id by `create_new` on the same path (see
//! [`claim_session_id`]), which is the only atomic way to reserve an id
//! whose log file does not exist yet.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

use crate::log::ConversationError;
use crate::persistence::ConversationPersistence;

/// The extension of a session's lock file.
const LOCK_FILE: &str = "lock";

/// Exclusive advisory lock on one session, held while it is live.
///
/// Released when the value is dropped, and by the kernel when the process
/// dies, so a crashed host leaves no stale lock behind.
///
/// NOTE: `flock(2)` locks belong to the open file description rather than
/// to the process, so two `try_acquire` calls in one process conflict
/// exactly as two processes would. The lock protects a session, not a
/// thread of control.
///
/// NOTE: the lock lives on a path, so unlinking the lock file while a
/// holder is live defeats it: the next acquire creates a fresh file and
/// locks that instead, and both processes then believe they hold the
/// session. Nothing in aj removes lock files, and we do not defend
/// against a hand-run `rm`.
///
/// The lock lives on a dedicated file rather than on the log because the
/// log is created lazily: a session that has not punctuated yet has no
/// file to lock.
pub struct SessionLock {
    /// Dropping this releases the lock.
    flock: Flock<File>,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        // Clear the holder record before the lock goes, while we still hold it.
        // A record that outlived its holder would name a process that has since
        // exited, or worse a live one that inherited its pid, so what a refusal
        // reports would be a guess dressed up as a fact. Surviving a clean
        // release is the one thing the record must not do: a crash leaves one
        // behind, and a crash also releases the lock, so nobody is refused and
        // nobody reads it.
        let _ = self.flock.set_len(0);
    }
}

/// Who holds a session's lock, as its lock file records it.
///
/// Display data, for telling a user which process to go quit or detach. It is
/// written best-effort and read back best-effort, so a missing, empty or
/// unparsable record is not an error, it just means the holder cannot be
/// named. Never branch on it: the lock itself is the only authority on
/// whether a session is held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockHolder {
    pub pid: u32,
    /// The store-level id of the host that took the lock, which is what
    /// distinguishes two hosts over one store (spec section 4).
    pub host_id: String,
}

impl fmt::Display for LockHolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pid {} of host {}", self.pid, self.host_id)
    }
}

impl SessionLock {
    /// Take the lock for `session_id`, or return `Ok(None)` when it is
    /// already held.
    ///
    /// Takes the persistence handle rather than a bare path so the lock
    /// cannot be sited in a different store than the log it guards.
    ///
    /// `host_id` identifies the taker for [`Self::holder`]. Recording it is
    /// best-effort: a lock we won but could not annotate is still a lock, so a
    /// failed write is logged and ignored rather than dropping it.
    pub fn try_acquire(
        persistence: &ConversationPersistence,
        session_id: &str,
        host_id: &str,
    ) -> Result<Option<Self>, ConversationError> {
        let path = lock_path(persistence.sessions_dir(), session_id)
            .ok_or_else(|| ConversationError::InvalidSessionId(session_id.to_string()))?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        // `create` rather than `create_new`: the file is already there
        // whenever `create` minted this id (see `claim_session_id`).
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(mut flock) => {
                // Only now, with the lock won: writing before would clobber
                // the record of whoever actually holds it.
                if let Err(err) = record_holder(&mut flock, host_id) {
                    tracing::debug!("could not record the holder of {}: {err}", path.display());
                }
                Ok(Some(Self { flock }))
            }
            // The documented non-blocking refusal, i.e. somebody else
            // holds it. Anything else is a real failure to report.
            Err((_, Errno::EWOULDBLOCK)) => Ok(None),
            Err((_, errno)) => Err(ConversationError::Io(errno.into())),
        }
    }

    /// The holder `session_id`'s lock file names, `None` when it names nobody
    /// legible: no file, no current holder, an older build's empty one, or a
    /// record we cannot parse.
    ///
    /// Ask only after an acquire of your own was refused. A record is cleared
    /// on release, so one that is present belongs to a live holder, but this
    /// does not check the lock itself and a holder that wrote none (an older
    /// build) leaves the answer empty rather than wrong.
    pub fn holder(persistence: &ConversationPersistence, session_id: &str) -> Option<LockHolder> {
        let path = lock_path(persistence.sessions_dir(), session_id)?;
        let recorded = fs::read_to_string(&path).ok()?;
        let (pid, host_id) = recorded.trim().split_once(char::is_whitespace)?;
        Some(LockHolder {
            pid: pid.parse().ok()?,
            host_id: host_id.trim().to_string(),
        })
    }

    /// Whether some writer holds `session_id`'s lock right now.
    ///
    /// A shared non-blocking probe, released before this returns. It is never
    /// the acquire path: it creates no file and writes nothing, so it stamps no
    /// holder record and cannot claim an unminted id. A session with no lock
    /// file reads free, which is the only thing the absence can mean.
    ///
    /// Two costs the caller accepts by asking at all. A shared lock conflicts
    /// with an exclusive one, so for the instant this holds a free lock it can
    /// refuse one racing acquire. And `flock` locks belong to the open file
    /// description rather than the process, so this reads a lock the calling
    /// process itself holds as held, exactly as a rival's: a caller that wants
    /// "held by somebody else" must exclude its own holdings before asking.
    pub fn is_held(
        persistence: &ConversationPersistence,
        session_id: &str,
    ) -> Result<bool, ConversationError> {
        let path = lock_path(persistence.sessions_dir(), session_id)
            .ok_or_else(|| ConversationError::InvalidSessionId(session_id.to_string()))?;
        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err.into()),
        };
        match Flock::lock(file, FlockArg::LockSharedNonblock) {
            // Won it, so nobody held it exclusively. Dropping the flock here
            // is what keeps the window this opens down to the probe itself.
            Ok(_) => Ok(false),
            Err((_, Errno::EWOULDBLOCK)) => Ok(true),
            Err((_, errno)) => Err(ConversationError::Io(errno.into())),
        }
    }

    /// Every lock file the store holds, one directory read plus a `stat` each.
    ///
    /// The sweep that keeps a directory's `locked` bits current (spec 6.8).
    /// [`LockMetadata::has_holder_record`] is the filter for which of them are
    /// worth a [`Self::is_held`] probe: a record is written under the won lock
    /// and truncated by a clean release, so an empty file is a lock nobody has
    /// taken since the last release of it. The record is never the answer, only
    /// the filter, because a holder that failed to write one leaves it empty
    /// while holding the lock.
    pub fn enumerate_locks(
        persistence: &ConversationPersistence,
    ) -> Result<Vec<LockMetadata>, ConversationError> {
        let files = persistence.session_files(&locks_dir(persistence.sessions_dir()), LOCK_FILE)?;
        Ok(files
            .into_iter()
            .map(|(session_id, metadata)| LockMetadata {
                session_id,
                has_holder_record: metadata.len() > 0,
            })
            .collect())
    }
}

/// One lock file in the store, as a sweep sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockMetadata {
    pub session_id: String,
    /// Whether the file carries a holder record, which is what a sweep filters
    /// on rather than probing every lock a store ever minted.
    pub has_holder_record: bool,
}

/// Stamp `pid host_id` onto a lock file we just won, replacing whatever the
/// previous holder left there.
fn record_holder(flock: &mut Flock<File>, host_id: &str) -> std::io::Result<()> {
    // Truncate rather than overwrite in place: the previous holder's record
    // can be longer than ours, and a partial overwrite would leave a hybrid
    // of two records behind.
    flock.set_len(0)?;
    // One write, because the reader takes no lock: a formatted write would land
    // in a syscall per fragment, and a read between two of them would see a pid
    // with no host id after it.
    flock.write_all(format!("{} {host_id}\n", std::process::id()).as_bytes())?;
    flock.flush()
}

/// Claim `session_id` by creating its lock file, failing with
/// [`std::io::ErrorKind::AlreadyExists`] when the id is taken.
///
/// This is the atomic reservation `ConversationLog::create` mints ids
/// with. The claim is deliberately not held open: it reserves the id, and
/// [`SessionLock::try_acquire`] later locks the same file for as long as
/// the session is live.
pub(crate) fn claim_session_id(sessions_dir: &Path, session_id: &str) -> std::io::Result<()> {
    let path = lock_path(sessions_dir, session_id).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{session_id:?} is not a session id"),
        )
    })?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .map(|_| ())
}

/// The lock file backing `session_id`, in a `locks/` subdirectory of the
/// sessions store, or `None` for an id the store's grammar rejects.
///
/// The grammar check sits here rather than at the callers because this is
/// where an id becomes a path, and taking a lock creates directories: an id
/// carrying `..` would make them outside the store (see [`crate::id`]).
pub(crate) fn lock_path(sessions_dir: &Path, session_id: &str) -> Option<PathBuf> {
    crate::id::is_valid_session_id(session_id)
        .then(|| locks_dir(sessions_dir).join(format!("{session_id}.{LOCK_FILE}")))
}

/// The directory holding the store's session locks.
fn locks_dir(sessions_dir: &Path) -> PathBuf {
    sessions_dir.join("locks")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Acquire for a fixed host id, since these tests do not care which.
    fn acquire(
        persistence: &ConversationPersistence,
        session_id: &str,
    ) -> Result<Option<SessionLock>, ConversationError> {
        SessionLock::try_acquire(persistence, session_id, "host-under-test")
    }

    #[test]
    fn an_id_the_grammar_rejects_creates_nothing() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));

        for id in ["../../escaped", "..", "", "with space"] {
            let err = acquire(&persistence, id)
                .err()
                .unwrap_or_else(|| panic!("{id:?} took a lock"));
            assert!(
                matches!(&err, ConversationError::InvalidSessionId(named) if named == id),
                "{id:?}: {err}",
            );
            assert!(SessionLock::holder(&persistence, id).is_none());
        }
        // Taking a lock creates the `locks/` directory it lives in, so a
        // refused acquire is only safe if it creates nothing at all.
        assert!(
            !dir.path().join("sessions").exists(),
            "a refused acquire made a directory",
        );
        assert!(
            !dir.path().join("locks").exists() && !dir.path().join("../locks").exists(),
            "a refused acquire escaped the store",
        );
    }

    #[test]
    fn a_held_lock_refuses_a_second_acquire_until_it_is_dropped() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));

        let held = acquire(&persistence, "s-1")
            .expect("acquire")
            .expect("nobody holds it yet");
        assert!(
            lock_path(persistence.sessions_dir(), "s-1")
                .expect("a well-formed id")
                .exists(),
            "the lock file lives in the sessions store's locks/ subdirectory"
        );

        // NOTE: `flock` locks belong to the open file description, not to
        // the process, so this second acquire on a fresh descriptor
        // conflicts on Linux exactly as another process would. That is
        // what makes an in-process assertion an honest test of the
        // cross-process behaviour we are after.
        assert!(
            acquire(&persistence, "s-1")
                .expect("try_acquire is not an error while held")
                .is_none(),
            "a second acquire must refuse while the first is held"
        );

        // A different session is a different lock file, so it is free.
        let other = acquire(&persistence, "s-2")
            .expect("acquire")
            .expect("a different session does not conflict");

        drop(held);
        let reacquired = acquire(&persistence, "s-1")
            .expect("acquire")
            .expect("the lock is free again once released");
        drop(reacquired);
        drop(other);
    }

    /// A claim reserves the id and still composes with the lock: the
    /// claim file is what `try_acquire` opens.
    #[test]
    fn a_claimed_id_is_refused_once_and_still_lockable() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));

        claim_session_id(persistence.sessions_dir(), "s-1").expect("first claim wins");
        let err = claim_session_id(persistence.sessions_dir(), "s-1")
            .expect_err("a second claim of the same id must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

        let held = acquire(&persistence, "s-1")
            .expect("acquire over an existing claim")
            .expect("a claim does not hold the lock");
        drop(held);
    }

    /// The refused side learns who to go quit: the holder's record is readable
    /// while the lock is held, and the lock file it lives in outlives the
    /// release (nothing removes one), so a caller only asks after a refusal of
    /// its own.
    #[test]
    fn a_taken_lock_records_a_holder_the_refused_side_can_read() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));

        assert_eq!(
            SessionLock::holder(&persistence, "s-1"),
            None,
            "a session nobody ever locked names nobody"
        );

        let held = SessionLock::try_acquire(&persistence, "s-1", "host-a")
            .expect("acquire")
            .expect("nobody holds it yet");
        assert!(acquire(&persistence, "s-1").expect("try_acquire").is_none());
        let holder = SessionLock::holder(&persistence, "s-1").expect("a recorded holder");
        assert_eq!(
            holder,
            LockHolder {
                pid: std::process::id(),
                host_id: "host-a".to_string(),
            },
        );
        assert_eq!(
            holder.to_string(),
            format!("pid {} of host host-a", std::process::id())
        );
        drop(held);

        // The next holder replaces the record rather than appending to it.
        let held = SessionLock::try_acquire(&persistence, "s-1", "b")
            .expect("acquire")
            .expect("free again");
        assert_eq!(
            SessionLock::holder(&persistence, "s-1")
                .expect("a recorded holder")
                .host_id,
            "b",
        );
        drop(held);
    }

    /// A lock file an older build left behind carries no record. It still
    /// locks, and the holder read reports nothing rather than failing.
    #[test]
    fn a_legacy_lock_file_names_nobody() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let path = lock_path(persistence.sessions_dir(), "s-1").expect("a well-formed id");
        fs::create_dir_all(path.parent().expect("a locks dir")).expect("mkdir");

        for content in ["", "  \n", "not-a-pid host", "12345", "\0\0"] {
            fs::write(&path, content).expect("write a legacy lock file");
            assert_eq!(
                SessionLock::holder(&persistence, "s-1"),
                None,
                "{content:?} names nobody legible"
            );
            let held = acquire(&persistence, "s-1")
                .expect("acquire over a legacy lock file")
                .expect("an unheld legacy lock file is lockable");
            drop(held);
        }
    }

    /// Releasing clears the record, so a record that is there belongs to a
    /// holder that has the lock now. One that outlived its holder would point
    /// a refused user at a process that has already exited, or at a live one
    /// that inherited its pid.
    #[test]
    fn releasing_a_lock_clears_its_holder_record() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));

        let held = SessionLock::try_acquire(&persistence, "s-1", "host-a")
            .expect("acquire")
            .expect("nobody holds it yet");
        // Recording the holder is best-effort. If it silently did nothing, the
        // read below would answer `None` for the wrong reason and this test
        // would measure nothing.
        assert_eq!(
            SessionLock::holder(&persistence, "s-1")
                .expect("the fixture must record a holder")
                .host_id,
            "host-a",
        );

        drop(held);

        assert_eq!(
            SessionLock::holder(&persistence, "s-1"),
            None,
            "the release left a record behind, which would name a dead holder",
        );
    }

    /// A crash releases the lock and leaves the dead holder's record behind,
    /// so the next holder has to replace it whole. Its own record is shorter
    /// whenever the dead host's id was longer, and a write that did not
    /// truncate first would leave the tail of the old one attached.
    #[test]
    fn a_record_a_crash_left_behind_is_replaced_whole() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let path = lock_path(persistence.sessions_dir(), "s-1").expect("a well-formed id");
        fs::create_dir_all(path.parent().expect("a locks dir")).expect("mkdir");

        let stale = format!("{} a-host-whose-id-is-long\n", std::process::id());
        let ours = format!("{} b\n", std::process::id());
        assert!(
            stale.len() > ours.len(),
            "the stale record is {} bytes and ours is {}: it has to be the longer \
             one, or an overwrite that never truncated would leave no tail behind \
             and this test measures nothing",
            stale.len(),
            ours.len(),
        );
        fs::write(&path, &stale).expect("a record a crash left behind");

        let held = SessionLock::try_acquire(&persistence, "s-1", "b")
            .expect("acquire")
            .expect("a crash releases the lock, so it is free");

        assert_eq!(
            SessionLock::holder(&persistence, "s-1"),
            Some(LockHolder {
                pid: std::process::id(),
                host_id: "b".to_string(),
            }),
            "the new record is a hybrid of two holders",
        );
        drop(held);
    }

    /// The probe answers the question the holder record cannot: who has the
    /// lock right now. A record is a hint left by a writer, the flock is the
    /// fact, and only the second one moves when a rival lets go.
    #[test]
    fn a_probe_reads_a_hold_and_its_release() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));

        let held = acquire(&persistence, "s-1")
            .expect("acquire")
            .expect("nobody holds it yet");
        assert!(
            SessionLock::is_held(&persistence, "s-1").expect("probe"),
            "a lock somebody holds reads held",
        );

        drop(held);
        assert!(
            !SessionLock::is_held(&persistence, "s-1").expect("probe"),
            "a released lock reads free, which is the transition the bit exists for",
        );
    }

    /// The probe must not create the file it asks about. A lock file is also
    /// the claim that reserves a session id ([`claim_session_id`]), so a probe
    /// that created one would mint an id nobody asked for and make the real
    /// creation of that id fail forever.
    #[test]
    fn a_probe_creates_no_lock_file() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let path = lock_path(persistence.sessions_dir(), "s-1").expect("a well-formed id");

        assert!(
            !SessionLock::is_held(&persistence, "s-1").expect("probe"),
            "a session with no lock file cannot be held by anybody",
        );
        assert!(!path.exists(), "the probe created the lock file");
        assert!(
            !path.parent().expect("a locks dir").exists(),
            "the probe created the locks directory",
        );
        // The harm the file would do, at the boundary where it lands: the id
        // is still there to be claimed.
        claim_session_id(persistence.sessions_dir(), "s-1")
            .expect("the probe left the id claimable");
    }

    /// The probe is not the acquire path, so it stamps no holder record. One
    /// that a crash left behind is evidence about that crash, and a probe that
    /// truncated or rewrote it would destroy the only name a later refusal has
    /// to report.
    #[test]
    fn a_probe_leaves_the_holder_record_alone() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let path = lock_path(persistence.sessions_dir(), "s-1").expect("a well-formed id");
        fs::create_dir_all(path.parent().expect("a locks dir")).expect("mkdir");
        let stale = format!("{} a-crashed-host\n", std::process::id());
        fs::write(&path, &stale).expect("a record a crash left behind");

        assert!(
            !SessionLock::is_held(&persistence, "s-1").expect("probe"),
            "a crash frees the lock however loud the record it left",
        );

        assert_eq!(
            fs::read_to_string(&path).expect("read the lock file"),
            stale,
            "the probe rewrote the record",
        );
    }

    /// The probe takes a shared lock to ask, so it has to give it back before
    /// returning. One that leaked would leave the session unacquirable for the
    /// life of the host that asked.
    #[test]
    fn a_probe_releases_what_it_took() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let claimed = acquire(&persistence, "s-1")
            .expect("acquire")
            .expect("nobody holds it yet");
        drop(claimed);

        for _ in 0..3 {
            assert!(!SessionLock::is_held(&persistence, "s-1").expect("probe"));
        }

        acquire(&persistence, "s-1")
            .expect("acquire")
            .expect("the probes released what they took");
    }

    /// The probe takes a *shared* lock, so two hosts sweeping one store do not
    /// read each other's asking as a hold. An exclusive probe would pass every
    /// other test here and report a free lock as held whenever another prober
    /// happened to be inside its own probe.
    #[test]
    fn a_probe_does_not_collide_with_another_probe() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let claimed = acquire(&persistence, "s-1")
            .expect("acquire")
            .expect("nobody holds it yet");
        drop(claimed);

        // Another prober, caught mid-probe: a shared lock on the same file,
        // held across the probe below.
        let path = lock_path(persistence.sessions_dir(), "s-1").expect("a well-formed id");
        let peer = OpenOptions::new().read(true).open(&path).expect("open");
        let peer = Flock::lock(peer, FlockArg::LockSharedNonblock)
            .map_err(|(_, errno)| errno)
            .expect("a free lock takes a shared probe");

        assert!(
            !SessionLock::is_held(&persistence, "s-1").expect("probe"),
            "a lock nobody holds read as held because another prober was asking",
        );

        drop(peer);
    }

    #[test]
    fn a_probe_refuses_an_id_the_grammar_rejects() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));

        for id in ["../../escaped", "..", "", "with space"] {
            let err = SessionLock::is_held(&persistence, id)
                .err()
                .unwrap_or_else(|| panic!("{id:?} was probed"));
            assert!(
                matches!(&err, ConversationError::InvalidSessionId(named) if named == id),
                "{id:?}: {err}",
            );
        }
        assert!(
            !dir.path().join("sessions").exists(),
            "a refused probe made a directory",
        );
    }

    /// The sweep's filter: one entry per lock file the store ever minted, and
    /// the record flag says which of them are worth a probe. A settled store
    /// carries records for nothing, so it is swept without probing at all.
    #[test]
    fn a_sweep_flags_the_locks_whose_records_are_worth_probing() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));

        assert_eq!(
            SessionLock::enumerate_locks(&persistence).expect("sweep a store with no locks"),
            Vec::new(),
            "a store nobody has locked has no locks directory to read",
        );

        // A minted id with no holder: the claim creates the file empty.
        claim_session_id(persistence.sessions_dir(), "s-1").expect("claim");
        let held = SessionLock::try_acquire(&persistence, "s-2", "host-a")
            .expect("acquire")
            .expect("nobody holds it yet");

        let swept = |persistence: &ConversationPersistence| {
            let mut locks = SessionLock::enumerate_locks(persistence).expect("sweep");
            locks.sort_by(|a, b| a.session_id.cmp(&b.session_id));
            locks
        };
        assert_eq!(
            swept(&persistence),
            vec![
                LockMetadata {
                    session_id: "s-1".to_string(),
                    has_holder_record: false,
                },
                LockMetadata {
                    session_id: "s-2".to_string(),
                    has_holder_record: true,
                },
            ],
            "a claim leaves an empty file, a hold leaves a record",
        );

        drop(held);
        assert!(
            swept(&persistence)
                .iter()
                .all(|lock| !lock.has_holder_record),
            "a clean release truncates its record, so a settled store probes nothing",
        );
    }

    /// The sweep reads the lock directory and only the lock directory: a
    /// stray file in there is not a lock, and neither is one whose name is not
    /// a session id.
    #[test]
    fn a_sweep_ignores_what_is_not_a_lock() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let held = acquire(&persistence, "s-1")
            .expect("acquire")
            .expect("nobody holds it yet");
        let locks = locks_dir(persistence.sessions_dir());

        fs::write(locks.join("notes.txt"), "not a lock").expect("write");
        fs::write(locks.join("../stray.lock"), "outside the locks dir").expect("write");
        fs::write(locks.join("with space.lock"), "not a session id").expect("write");
        fs::create_dir_all(locks.join("s-2.lock")).expect("a directory named like a lock");

        assert_eq!(
            SessionLock::enumerate_locks(&persistence)
                .expect("sweep")
                .into_iter()
                .map(|lock| lock.session_id)
                .collect::<Vec<_>>(),
            vec!["s-1".to_string()],
        );
        drop(held);
    }
}
