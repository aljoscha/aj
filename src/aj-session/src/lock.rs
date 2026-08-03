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

use std::fs::{self, File, OpenOptions};
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
    /// Dropping this releases the lock. Held for that effect only.
    _flock: Flock<File>,
}

impl SessionLock {
    /// Take the lock for `session_id`, or return `Ok(None)` when it is
    /// already held.
    ///
    /// Takes the persistence handle rather than a bare path so the lock
    /// cannot be sited in a different store than the log it guards.
    pub fn try_acquire(
        persistence: &ConversationPersistence,
        session_id: &str,
    ) -> Result<Option<Self>, ConversationError> {
        let path = lock_path(persistence.sessions_dir(), session_id);
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
            Ok(flock) => Ok(Some(Self { _flock: flock })),
            // The documented non-blocking refusal, i.e. somebody else
            // holds it. Anything else is a real failure to report.
            Err((_, Errno::EWOULDBLOCK)) => Ok(None),
            Err((_, errno)) => Err(ConversationError::Io(errno.into())),
        }
    }
}

/// Claim `session_id` by creating its lock file, failing with
/// [`std::io::ErrorKind::AlreadyExists`] when the id is taken.
///
/// This is the atomic reservation `ConversationLog::create` mints ids
/// with. The claim is deliberately not held open: it reserves the id, and
/// [`SessionLock::try_acquire`] later locks the same file for as long as
/// the session is live.
pub(crate) fn claim_session_id(sessions_dir: &Path, session_id: &str) -> std::io::Result<()> {
    let path = lock_path(sessions_dir, session_id);
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
/// sessions store.
pub(crate) fn lock_path(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir
        .join("locks")
        .join(format!("{session_id}.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_held_lock_refuses_a_second_acquire_until_it_is_dropped() {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));

        let held = SessionLock::try_acquire(&persistence, "s-1")
            .expect("acquire")
            .expect("nobody holds it yet");
        assert!(
            lock_path(persistence.sessions_dir(), "s-1").exists(),
            "the lock file lives in the sessions store's locks/ subdirectory"
        );

        // NOTE: `flock` locks belong to the open file description, not to
        // the process, so this second acquire on a fresh descriptor
        // conflicts on Linux exactly as another process would. That is
        // what makes an in-process assertion an honest test of the
        // cross-process behaviour we are after.
        assert!(
            SessionLock::try_acquire(&persistence, "s-1")
                .expect("try_acquire is not an error while held")
                .is_none(),
            "a second acquire must refuse while the first is held"
        );

        // A different session is a different lock file, so it is free.
        let other = SessionLock::try_acquire(&persistence, "s-2")
            .expect("acquire")
            .expect("a different session does not conflict");

        drop(held);
        let reacquired = SessionLock::try_acquire(&persistence, "s-1")
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

        let held = SessionLock::try_acquire(&persistence, "s-1")
            .expect("acquire over an existing claim")
            .expect("a claim does not hold the lock");
        drop(held);
    }
}
