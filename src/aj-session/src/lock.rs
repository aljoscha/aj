//! Advisory single-writer lock for one session.
//!
//! Materializing a session in a host process takes this lock and holds it
//! for the session's live lifetime, so a second process that tries the
//! same session refuses rather than growing a sibling branch in a shared
//! log (spec section 5).

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

use crate::log::ConversationError;

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
/// The lock lives on a dedicated file rather than on the log, because the
/// log is created lazily (a session that never punctuates has no file to
/// lock), and in a `locks/` subdirectory so session listing does not have
/// to filter it out.
pub struct SessionLock {
    /// Dropping this releases the lock. Held for that effect only.
    _flock: Flock<File>,
    path: PathBuf,
}

impl SessionLock {
    /// Take the lock for `session_id`, or return `Ok(None)` when it is
    /// already held.
    pub fn try_acquire(
        sessions_dir: &Path,
        session_id: &str,
    ) -> Result<Option<Self>, ConversationError> {
        let dir = sessions_dir.join("locks");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{session_id}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(flock) => Ok(Some(Self {
                _flock: flock,
                path,
            })),
            // The documented non-blocking refusal, i.e. somebody else
            // holds it. Anything else is a real failure to report.
            Err((_, Errno::EWOULDBLOCK)) => Ok(None),
            Err((_, errno)) => Err(ConversationError::Io(errno.into())),
        }
    }

    /// The lock file backing this lock.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_held_lock_refuses_a_second_acquire_until_it_is_dropped() {
        let dir = TempDir::new().expect("temp dir");
        let sessions = dir.path().join("sessions");

        let held = SessionLock::try_acquire(&sessions, "s-1")
            .expect("acquire")
            .expect("nobody holds it yet");
        assert_eq!(
            held.path(),
            sessions.join("locks").join("s-1.lock"),
            "the lock file lives in a subdirectory, out of the session listing"
        );

        // NOTE: `flock` locks belong to the open file description, not to
        // the process, so this second acquire on a fresh descriptor
        // conflicts on Linux exactly as another process would. That is
        // what makes an in-process assertion an honest test of the
        // cross-process behaviour we are after.
        assert!(
            SessionLock::try_acquire(&sessions, "s-1")
                .expect("try_acquire is not an error while held")
                .is_none(),
            "a second acquire must refuse while the first is held"
        );

        // A different session is a different lock file, so it is free.
        let other = SessionLock::try_acquire(&sessions, "s-2")
            .expect("acquire")
            .expect("a different session does not conflict");

        drop(held);
        let reacquired = SessionLock::try_acquire(&sessions, "s-1")
            .expect("acquire")
            .expect("the lock is free again once released");
        drop(reacquired);
        drop(other);
    }
}
