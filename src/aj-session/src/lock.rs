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
    crate::id::is_valid_session_id(session_id).then(|| {
        sessions_dir
            .join("locks")
            .join(format!("{session_id}.lock"))
    })
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
}
