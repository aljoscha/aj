//! Spawning the child process on a [`Pty`](crate::widgets::terminal::pty::Pty)
//! slave.
//!
//! Upstream forks and `execvpe`s by hand, wiring up the controlling terminal in
//! the child between the fork and the exec. We express the same discipline
//! through [`std::process::Command`]: `stdin`/`stdout`/`stderr` are set to
//! dup'd slave fds, and a [`CommandExt::pre_exec`] hook runs `setsid` +
//! `TIOCSCTTY` in the child after fork but before exec. `std` handles the
//! `execvpe` (PATH search, environment) and the `dup2` of our stdio fds onto
//! 0/1/2 for us.
//!
//! ## Post-fork / pre-exec discipline
//!
//! The `pre_exec` closure runs in the forked child, which may share address
//! space semantics with a multi-threaded parent, so it must be
//! async-signal-safe: it allocates nothing and calls only `setsid` and the
//! `TIOCSCTTY` ioctl (both on the async-signal-safe list). It does not touch
//! the stdio fds, `std` has already `dup2`'d the slave onto 0/1/2 by the time
//! the hook runs, and the ioctl targets the inherited slave fd directly.

#![cfg(unix)]

use std::ffi::OsString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Stdio};

use thiserror::Error;

// TIOCSCTTY takes an int argument by value (0 = do not steal from another
// session), so it is an `int_bad` ioctl. The macro casts the request code.
nix::ioctl_write_int_bad!(tiocsctty, nix::libc::TIOCSCTTY);

/// A failure spawning the child.
#[derive(Debug, Error)]
pub enum CommandError {
    /// Duplicating the slave fd for one of the child's stdio streams failed.
    #[error("failed to duplicate the pty slave for the child's stdio")]
    DupSlave(#[source] std::io::Error),

    /// `fork`/`exec` of the child failed.
    #[error("failed to spawn the child process")]
    Spawn(#[source] std::io::Error),
}

/// A child command to run inside the terminal.
///
/// `program` is `argv[0]` and drives the PATH search, `args` are the rest.
pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    working_directory: Option<PathBuf>,
}

impl Command {
    /// Builds a command from a program and its arguments.
    pub fn new(program: OsString, args: Vec<OsString>) -> Command {
        Command {
            program,
            args,
            working_directory: None,
        }
    }

    /// Sets the child's initial working directory.
    pub fn set_working_directory(&mut self, dir: PathBuf) {
        self.working_directory = Some(dir);
    }

    /// Forks and execs the child on `slave`, returning the running process.
    ///
    /// Three dup'd copies of the slave become the child's stdin/stdout/stderr.
    /// The `pre_exec` hook makes the child a session leader and claims the slave
    /// as its controlling terminal. The caller owns the returned [`Child`] and
    /// is responsible for reaping it.
    pub fn spawn(&self, slave: &OwnedFd) -> Result<Child, CommandError> {
        let stdin = slave.try_clone().map_err(CommandError::DupSlave)?;
        let stdout = slave.try_clone().map_err(CommandError::DupSlave)?;
        let stderr = slave.try_clone().map_err(CommandError::DupSlave)?;

        // The raw slave fd is inherited across the fork and stays open when the
        // pre_exec hook runs (exec, which would honor CLOEXEC, has not happened
        // yet), so the ioctl can target it directly without allocation.
        let slave_fd = slave.as_raw_fd();

        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        if let Some(dir) = &self.working_directory {
            cmd.current_dir(dir);
        }

        // SAFETY: the hook only calls `setsid` and the `TIOCSCTTY` ioctl, both
        // async-signal-safe, and allocates nothing on the success path.
        let hook = move || -> std::io::Result<()> {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            // SAFETY: `slave_fd` is an inherited, open tty fd in the child.
            unsafe { tiocsctty(slave_fd, 0).map_err(std::io::Error::from)? };
            Ok(())
        };
        // SAFETY: `pre_exec` requires an async-signal-safe hook, which `hook`
        // is.
        unsafe {
            cmd.pre_exec(hook);
        }

        cmd.spawn().map_err(CommandError::Spawn)
    }
}
