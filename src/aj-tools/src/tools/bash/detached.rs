//! The runner of every bash command: a process in a session of its own.
//!
//! The command runs under a small shell wrapper. Its output goes to a file
//! and its exit status is written to a second file when it ends. The
//! process that started it, or for a background task a later one, watches
//! the two files and the pid (see [`DetachedProcess`]).
//!
//! The wrapper waits for a line on its stdin before it starts the command,
//! so a process that dies right after the spawn leaves no command running.
//!
//! A background command ([`launch`]) is a task with a driver. A session
//! with a [`aj_agent::TaskStore`] records it before it runs: the line is
//! sent once the record is saved, and a launch that cannot be recorded
//! runs nothing. From then on the command outlives this process. Without a
//! store nothing could find the command again, so it ends with its
//! registry.
//!
//! A foreground command ([`run`]) is waited for by its tool call, which
//! reports its end as the tool result. Nothing records it, and it never
//! outlives this process: the call holds the wrapper's stdin open, and the
//! wrapper kills the command's process group when that pipe closes (see
//! [`WRAPPER`]). The system closes the pipe when this process ends, however
//! it ends.
//!
//! What follows from the wrapper, for both:
//!
//! - **One output stream.** Stdout and stderr share one file, in the order
//!   the command wrote them. The tail and the byte total are of both, and
//!   are reported as stdout.
//! - **Exit status.** It is what the shell reports, so a command killed by
//!   signal `N` ends with exit code `128 + N`.
//! - **Descendants.** The command ends when its shell does. A process it
//!   left behind keeps running and keeps the output file open. A kill
//!   signals the command's whole process group.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_agent::TaskCleanupGuard;
use aj_agent::events::AgentId;
use aj_agent::tool::{
    StartedTask, TaskEventSink, TaskId, TaskKind, TaskNotice, TaskOutputSource, TaskRead,
    TaskStatus, ToolContext, ToolDetails, ToolOutcome,
};
use aj_models::types::UserContent;
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{
    BashResult, BashTool, ChildExit, KILL_GRACE, ROLLING_CAP_BYTES, StreamState, UPDATE_DEBOUNCE,
    apply_environment, background_terminal_status, build_wire_content, decode_stream_output,
    finalize_stream, push_marker, render_stream_block, skipped_stream_marker, snapshot_partial,
    spawn_error_outcome, task_status_text,
};

/// Runs `$1` once a line arrives on stdin, and then writes its exit status
/// to `$2` as one line. A file without a whole line is not a status yet
/// (see [`DetachedProcess::exit_code`]).
///
/// The line is `go` or `hold`. After `go` the wrapper never reads stdin
/// again. After `hold` the pipe is the command's lifeline: a watcher reads
/// it, and kills the whole process group when the pipe closes while the
/// command runs. Nothing is ever written after the line, so the pipe closes
/// when its holder lets go of it or dies.
///
/// The watcher ignores the polite termination signals, so a `SIGTERM` to
/// the group does not disarm the lifeline for a command that ignores the
/// signal too. The signals are set to ignored before the watcher is forked
/// and trapped again after, so there is no moment at which one could end
/// it. The wrapper ends the watcher with `SIGKILL` and waits for it before
/// it writes the status: a command that has ended leaves no process of the
/// wrapper's in its group.
///
/// The wrapper uses shell builtins and its own shell only, so it works
/// whatever `PATH` the session gives its commands.
///
/// The polite termination signals are trapped rather than ignored: a
/// signal to the process group stops the command, which gets the default
/// dispositions back, and leaves the wrapper to record the status.
///
/// The wrapper's own stderr is discarded and the command gets the original.
/// Otherwise the shell's report that its child was terminated would land in
/// the command's output.
pub(crate) const WRAPPER: &str = r#"trap : HUP INT TERM
IFS= read -r go || exit 125
exec 3>&2 2>/dev/null
case $go in
go) w= ;;
hold)
  trap '' HUP INT TERM
  { IFS= read -r _; kill -s KILL 0; } <&0 3>&- &
  w=$!
  trap : HUP INT TERM ;;
*) exit 125 ;;
esac
"$BASH" -c "$1" </dev/null 2>&3 3>&-
s=$?
[ -z "$w" ] || { kill -s KILL "$w"; wait "$w"; }
printf '%d\n' "$s" >"$2"
exit "$s""#;

/// The longest a quiet command goes without a look at its output file.
/// Output that resumes after a quiet spell reaches the UI and `task_output`
/// within this. It is also how often a command is looked at when nothing
/// reports its end (see [`Exit::Polled`]).
const QUIET_POLL: Duration = Duration::from_secs(1);

/// The wait between two reads of an output file. It doubles from
/// [`UPDATE_DEBOUNCE`] to [`QUIET_POLL`] while the file does not grow, so a
/// quiet command costs one empty read per [`QUIET_POLL`]. Growth sets it
/// back, so flowing output is shown at the debounce rate.
struct Pace(Duration);

impl Pace {
    fn new() -> Self {
        Self(UPDATE_DEBOUNCE)
    }

    /// The wait after a read, given whether it found the file grown.
    fn after(&mut self, grew: bool) -> Duration {
        self.0 = if grew {
            UPDATE_DEBOUNCE
        } else {
            (self.0 * 2).min(QUIET_POLL)
        };
        self.0
    }
}

/// The most output read between two looks at the command, so catching up
/// on a long file does not hold the driver's thread.
const READ_BUDGET: usize = 4 * 1024 * 1024;

/// What finds a detached command again: the wrapper's process and the two
/// files it writes. This is the state a [`aj_agent::TaskStore`] records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetachedProcess {
    /// The wrapper's pid, which is also its process group and session.
    pub pid: u32,
    /// When the wrapper started, as the system counts it (see
    /// [`process_start_time`]). A process with this pid and another start
    /// time is a later, unrelated one.
    pub start_time: u64,
    /// Everything the command writes to stdout and stderr.
    pub output_path: PathBuf,
    /// The command's exit status, once it has ended.
    pub exit_path: PathBuf,
}

impl DetachedProcess {
    /// Whether the wrapper is still running: a process with this pid that
    /// leads its process group and started at the recorded time. A later
    /// process that took the pid is not it.
    pub fn alive(&self) -> bool {
        process_start_time(self.pid) == Some(self.start_time)
    }

    /// The recorded exit status, `None` while there is none.
    fn exit_code(&self) -> Option<i32> {
        exit_code_in(&self.exit_path)
    }

    /// Whether any process is left in the command's process group.
    fn group_alive(&self) -> bool {
        #[cfg(unix)]
        {
            use nix::errno::Errno;
            use nix::sys::signal::killpg;
            let Some(pgid) = self.pgid() else {
                return false;
            };
            !matches!(killpg(pgid, None), Err(Errno::ESRCH))
        }
        #[cfg(not(unix))]
        self.alive()
    }

    #[cfg(unix)]
    fn pgid(&self) -> Option<nix::unistd::Pid> {
        // Group 0 is our own and group 1 is the broadcast to everything.
        let pid = i32::try_from(self.pid).ok().filter(|pid| *pid > 1)?;
        Some(nix::unistd::Pid::from_raw(pid))
    }

    /// Stop the command: `SIGTERM` to its process group, and `SIGKILL`
    /// after [`KILL_GRACE`] to whatever is left. Returns once the group is
    /// empty or another [`KILL_GRACE`] has passed.
    ///
    /// Nothing is signalled unless the wrapper is still the recorded
    /// process. While it lives its pid cannot be given to another process,
    /// so the group signalled is the command's.
    async fn stop(&self) {
        if !self.alive() {
            return;
        }
        self.signal(false);
        self.escalate().await;
    }

    /// The rest of [`Self::stop`] once `SIGTERM` is sent.
    async fn escalate(&self) {
        if self.group_gone_within(KILL_GRACE).await {
            return;
        }
        self.signal(true);
        self.group_gone_within(KILL_GRACE).await;
    }

    fn signal(&self, kill: bool) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid() {
            use nix::sys::signal::{Signal, killpg};
            let signal = if kill {
                Signal::SIGKILL
            } else {
                Signal::SIGTERM
            };
            let _ = killpg(pgid, signal);
        }
        #[cfg(not(unix))]
        let _ = kill;
    }

    async fn group_gone_within(&self, grace: Duration) -> bool {
        let deadline = Instant::now() + grace;
        while self.group_alive() {
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }

    /// Remove the exit file, which nothing reads once the task's end is
    /// saved. The output file stays for the model to read.
    pub fn discard_exit_file(&self) {
        let _ = std::fs::remove_file(&self.exit_path);
        let _ = std::fs::remove_file(exit_tmp_path(&self.exit_path));
    }
}

/// The exit status the file holds: one whole line. A file the wrapper has
/// created and not written yet holds none.
fn exit_code_in(exit_path: &Path) -> Option<i32> {
    std::fs::read_to_string(exit_path)
        .ok()?
        .strip_suffix('\n')?
        .parse()
        .ok()
}

fn exit_tmp_path(exit_path: &Path) -> PathBuf {
    let mut name = exit_path.as_os_str().to_owned();
    name.push(".tmp");
    name.into()
}

/// When the process group leader `pid` started, in a unit that only
/// compares equal for the same process: two processes that have the same
/// pid at different times have different start times. `None` when no such
/// process is running. One that has exited and is not reaped yet is not
/// running, and one that leads no process group is not a wrapper.
///
/// On Linux this is the start time in clock ticks since boot, which tells
/// processes of one boot apart. After a reboot another process could have
/// the same pid at the same tick. Requiring a group leader rules out most
/// of those.
#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may itself hold spaces and
    // parentheses, so the fields are counted from the last one.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_ascii_whitespace().collect();
    // After the name: state, ppid, pgrp, ..., and starttime as the 20th.
    if matches!(*fields.first()?, "Z" | "X") {
        return None;
    }
    if fields.get(2)?.parse::<u32>().ok()? != pid {
        return None;
    }
    fields.get(19)?.parse().ok()
}

/// As on Linux, in microseconds of wall-clock time.
#[cfg(target_os = "macos")]
fn process_start_time(pid: u32) -> Option<u64> {
    const SZOMB: u32 = 5;
    let pid = libc::c_int::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is a writable buffer of `size` bytes, which is all
    // the call is told it may fill.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    // SAFETY: the call reported that it filled the whole structure.
    let info = unsafe { info.assume_init() };
    if info.pbi_status == SZOMB || libc::c_int::try_from(info.pbi_pgid).ok() != Some(pid) {
        return None;
    }
    Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

/// Elsewhere a start time is not available, so a live pid is taken for the
/// process it was.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_start_time(pid: u32) -> Option<u64> {
    #[cfg(unix)]
    {
        let pid = nix::unistd::Pid::from_raw(i32::try_from(pid).ok()?);
        nix::sys::signal::kill(pid, None).ok().map(|()| 0)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

/// The output file as it has been read so far.
struct Tail {
    file: Option<std::fs::File>,
    path: PathBuf,
    state: Arc<Mutex<StreamState>>,
    /// Whether the first read starts near the end of the file (see
    /// [`Self::from_end`]).
    from_end: bool,
    /// The first failure to read the file. Reading stops there.
    error: Option<String>,
    /// How often the file was looked at.
    #[cfg(test)]
    polls: Arc<std::sync::atomic::AtomicU64>,
}

impl Tail {
    /// Read the file from its first byte, as the process that started the
    /// command does. Every line is counted.
    fn new(path: PathBuf) -> Self {
        Self {
            file: None,
            path,
            state: Arc::new(Mutex::new(StreamState::new())),
            from_end: false,
            error: None,
            #[cfg(test)]
            polls: Arc::default(),
        }
    }

    /// Read a file an earlier process let grow: only its last
    /// [`ROLLING_CAP_BYTES`], which is all a rolling tail keeps, so the
    /// cost does not depend on what the command has written. The byte
    /// total is still of the whole file. The lines before the window are
    /// not counted, and the tail may start in the middle of a line.
    fn from_end(path: PathBuf) -> Self {
        Self {
            from_end: true,
            ..Self::new(path)
        }
    }

    fn open(&self) -> std::io::Result<std::fs::File> {
        let mut file = std::fs::File::open(&self.path)?;
        let window = u64::try_from(ROLLING_CAP_BYTES).unwrap_or(u64::MAX);
        let len = file.metadata()?.len();
        if self.from_end && len > window {
            let skipped = len - window;
            // The byte before the window says whether it starts a line.
            let mut before = [0];
            file.seek(SeekFrom::Start(skipped - 1))?;
            file.read_exact(&mut before)?;
            self.state.lock().unwrap().skip(skipped, before[0] == b'\n');
        }
        Ok(file)
    }

    /// Read what the file gained, up to [`READ_BUDGET`]. Returns whether
    /// the budget ran out with more to read.
    fn poll(&mut self) -> bool {
        #[cfg(test)]
        self.polls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.error.is_some() {
            return false;
        }
        match self.read() {
            Ok(more) => more,
            Err(error) => {
                self.error = Some(format!("{}: {error}", self.path.display()));
                false
            }
        }
    }

    fn read(&mut self) -> std::io::Result<bool> {
        let file = match &mut self.file {
            Some(file) => file,
            None => {
                let file = self.open()?;
                self.file.insert(file)
            }
        };
        let mut buffer = vec![0; 64 * 1024];
        let mut read = 0;
        while read < READ_BUDGET {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                return Ok(false);
            }
            self.state.lock().unwrap().append_chunk(&buffer[..n]);
            read += n;
        }
        Ok(true)
    }

    fn read_to_end(&mut self) {
        while self.poll() {}
    }

    fn total_bytes(&self) -> u64 {
        self.state.lock().unwrap().total_bytes_seen
    }
}

/// [`TaskOutputSource`] over a detached command's output file as its
/// driver has read it. The file is the full output.
struct DetachedOutput {
    state: Arc<Mutex<StreamState>>,
    path: PathBuf,
}

impl TaskOutputSource for DetachedOutput {
    fn snapshot(&self) -> TaskRead {
        let (stdout_tail, stdout_total_bytes) = tail_snapshot(&self.state);
        TaskRead {
            stdout_tail,
            stdout_total_bytes,
            spill_path: Some(self.path.clone()),
            ..TaskRead::default()
        }
    }
}

/// Decode a stream's rolling tail plus its exact byte total. Mirrors
/// `finalize_stream`'s whole-line policy: a leading partial line left
/// by a mid-line trim is dropped, except when the tail has no newline
/// at all (a single huge line stays visible).
fn tail_snapshot(state: &Arc<Mutex<StreamState>>) -> (String, u64) {
    let (bytes, at_boundary, total) = {
        let s = state.lock().unwrap();
        (
            s.tail.clone(),
            s.tail_starts_at_boundary,
            s.total_bytes_seen,
        )
    };
    let decoded = decode_stream_output(bytes);
    let text = if at_boundary {
        decoded
    } else {
        match decoded.find('\n') {
            None => decoded,
            Some(idx) => decoded[idx + 1..].to_string(),
        }
    };
    (text, total)
}

/// How the driver learns that the command ended.
enum Watch {
    /// This process started the wrapper and reaps it. The sender fires, or
    /// is dropped, once the wrapper is gone and its exit file is written.
    Child(tokio::sync::oneshot::Receiver<()>),
    /// Another process started the wrapper, so this one cannot reap it.
    Pid(Exit),
}

/// How the end of a wrapper this process did not start is noticed.
#[derive(Default)]
enum Exit {
    /// Not decided yet. The first wait decides, on the runtime.
    #[default]
    Unwatched,
    /// A descriptor of the wrapper's process, readable once it has exited.
    /// Waiting on it costs nothing while the command runs.
    #[cfg(target_os = "linux")]
    Fd(tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>),
    /// Nothing reports the exit, so the process is looked at every
    /// [`QUIET_POLL`] and whenever its driver wakes.
    Polled,
}

impl Exit {
    fn of(process: &DetachedProcess) -> Self {
        #[cfg(target_os = "linux")]
        if let Some(fd) = pidfd_open(process.pid)
            && let Ok(fd) =
                tokio::io::unix::AsyncFd::with_interest(fd, tokio::io::Interest::READABLE)
            // The descriptor is of whichever process had the pid when it
            // was opened. The wrapper keeps its pid while it lives, so if
            // it is alive now, the descriptor is the wrapper's.
            && process.alive()
        {
            return Self::Fd(fd);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = process;
        Self::Polled
    }
}

/// A descriptor of process `pid` that becomes readable when the process
/// exits, whoever its parent is. `None` when there is no such process, or
/// the kernel predates the call (Linux 5.3).
#[cfg(target_os = "linux")]
fn pidfd_open(pid: u32) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};
    let pid = libc::pid_t::try_from(pid).ok()?;
    // SAFETY: the call takes a pid and flags by value and touches no
    // memory of this process.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    let fd = RawFd::try_from(fd).ok().filter(|fd| *fd >= 0)?;
    // SAFETY: the call returned a new open descriptor that nothing else
    // owns. It is close-on-exec by definition.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

impl Watch {
    /// Resolves once the command has ended: its exit file holds a status,
    /// or its wrapper is gone. Dropping the future and calling again is
    /// cheap, and loses nothing.
    async fn ended(&mut self, process: &DetachedProcess) {
        match self {
            Self::Child(reaped) => {
                let _ = reaped.await;
            }
            Self::Pid(exit) => {
                if matches!(exit, Exit::Unwatched) {
                    *exit = Exit::of(process);
                }
                #[cfg(target_os = "linux")]
                if let Exit::Fd(fd) = exit {
                    if fd.readable().await.is_ok() {
                        // An exited wrapper has written its status, unless
                        // it was killed outright. Then the process that
                        // started it writes one (see [`reap`]). That may be
                        // this process under an earlier driver, and its
                        // reaper wakes when this does.
                        if process.exit_code().is_none() {
                            tokio::time::sleep(UPDATE_DEBOUNCE).await;
                        }
                        return;
                    }
                    *exit = Exit::Polled;
                }
                loop {
                    // The wrapper writes the exit file before it exits, so
                    // liveness is sampled first: a wrapper seen gone has
                    // written whatever it was going to write.
                    let alive = process.alive();
                    if process.exit_code().is_some() || !alive {
                        return;
                    }
                    tokio::time::sleep(QUIET_POLL).await;
                }
            }
        }
    }

    /// After a stop, let the reaper finish with the exit file before
    /// anyone discards it.
    async fn reaped(&mut self) {
        if let Self::Child(reaped) = self {
            let _ = tokio::time::timeout(KILL_GRACE, reaped).await;
        }
    }
}

/// Wait for the wrapper this process started, and write the exit file
/// itself when the wrapper could not, having been killed outright.
///
/// A wrapper that exited by itself is never written for. It wrote its own
/// status, and a driver that re-attached by pid may already have reported
/// the end and discarded the file by the time this runs.
async fn reap(mut child: Child, exit_path: PathBuf, reaped: tokio::sync::oneshot::Sender<()>) {
    #[cfg(unix)]
    let signal = child
        .wait()
        .await
        .ok()
        .and_then(|status| std::os::unix::process::ExitStatusExt::signal(&status));
    #[cfg(not(unix))]
    let signal: Option<i32> = child.wait().await.ok().and(None);
    if let Some(signal) = signal
        && exit_code_in(&exit_path).is_none()
    {
        let tmp = exit_tmp_path(&exit_path);
        if std::fs::write(&tmp, format!("{}\n", 128 + signal)).is_ok() {
            let _ = std::fs::rename(&tmp, &exit_path);
        }
    }
    let _ = reaped.send(());
}

/// Kill and reap a wrapper that was never sent its line.
fn abandon(mut child: Child) {
    let _ = child.start_kill();
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
}

/// A wrapper that is running and waits for its line.
struct Spawned {
    child: Child,
    /// The wrapper's stdin. It runs nothing until it is sent its line, and
    /// exits when the pipe closes without one. So every way out before
    /// [`Self::go`] only has to drop this, and so does a crash.
    gate: tokio::process::ChildStdin,
    process: DetachedProcess,
    /// `process` as a [`aj_agent::TaskStore`] records it.
    state: serde_json::Value,
}

impl Spawned {
    /// Start a wrapper for `executed`, shown to the model as `command`.
    /// The inner `Err` is the result of a call whose command could not be
    /// started.
    fn start(
        tool: &BashTool,
        ctx: &dyn ToolContext,
        command: &str,
        executed: &str,
        working_dir: &Path,
    ) -> Result<Result<Self, ToolOutcome>, aj_agent::BoxError> {
        let (log, output_path) = tool.create_task_log()?;
        let exit_path = output_path.with_extension("exit");
        let discard = || {
            let _ = std::fs::remove_file(&output_path);
        };

        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(WRAPPER)
            .arg("aj-task")
            .arg(executed)
            .arg(&exit_path)
            .current_dir(working_dir)
            .stdin(Stdio::piped())
            .stdout(log.try_clone()?)
            .stderr(log);
        apply_environment(&mut cmd, ctx.session_env());
        #[cfg(unix)]
        // SAFETY: `setsid` is async-signal-safe and touches no memory of the
        // parent. A session of its own takes the command out of this process's
        // process group and away from its terminal, so neither a signal to the
        // group nor the terminal closing reaches it.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid()
                    .map(drop)
                    .map_err(std::io::Error::from)
            });
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                discard();
                return Ok(Err(spawn_error_outcome(
                    command,
                    format!("Failed to start command '{command}': {error}"),
                )));
            }
        };

        let gate = child.stdin.take().expect("stdin was piped above");
        let found = || -> Result<_, aj_agent::BoxError> {
            let process = child
                .id()
                .and_then(|pid| {
                    Some(DetachedProcess {
                        pid,
                        start_time: process_start_time(pid)?,
                        output_path: output_path.clone(),
                        exit_path: exit_path.clone(),
                    })
                })
                .ok_or("the command exited before it was started")?;
            let state = serde_json::to_value(&process)?;
            Ok((process, state))
        };
        match found() {
            Ok((process, state)) => Ok(Ok(Self {
                child,
                gate,
                process,
                state,
            })),
            Err(error) => {
                drop(gate);
                abandon(child);
                discard();
                Err(error)
            }
        }
    }

    /// Let the command start as one that outlives this process, and reap
    /// its wrapper for as long as this process lives, whoever goes on
    /// watching the command.
    fn go(self) -> (DetachedProcess, Watch) {
        let (process, watch, _) = self.start_with("go");
        (process, watch)
    }

    /// Let the command start as one that ends with this process, and reap
    /// its wrapper. The command's process group is killed when the
    /// returned [`Lifeline`] is dropped, and when this process ends.
    fn hold(self) -> (DetachedProcess, Watch, Lifeline) {
        self.start_with("hold")
    }

    /// A failed send means the wrapper is already gone, or goes now that
    /// its pipe closes without a line. Its watcher reports that like any
    /// other end.
    fn start_with(self, line: &str) -> (DetachedProcess, Watch, Lifeline) {
        let gate = self
            .gate
            .into_owned_fd()
            .map(std::fs::File::from)
            .and_then(|mut gate| writeln!(gate, "{line}").map(|()| gate));
        let (reaped, watch) = tokio::sync::oneshot::channel();
        tokio::spawn(reap(self.child, self.process.exit_path.clone(), reaped));
        let lifeline = Lifeline { _pipe: gate.ok() };
        (self.process, Watch::Child(watch), lifeline)
    }

    /// Give up a wrapper that was never sent its line, and its file.
    fn abandon(self) {
        drop(self.gate);
        abandon(self.child);
        let _ = std::fs::remove_file(&self.process.output_path);
    }
}

/// The write end of the stdin pipe of a wrapper that was told to `hold`
/// (see [`WRAPPER`]). The command's process group is killed when this is
/// dropped while the command runs. Dropping it after the command ended does
/// nothing.
///
/// The descriptor is close-on-exec, as the pipe was created: no other
/// child of this process holds the pipe open past its own `exec`.
struct Lifeline {
    _pipe: Option<std::fs::File>,
}

/// The result of a call whose command runs as background task `id`.
fn task_outcome(
    lead: &str,
    command: String,
    id: TaskId,
    output_path: PathBuf,
) -> Result<ToolOutcome, aj_agent::BoxError> {
    let wire = format!(
        "{lead} #{id}: {command}\n\
         Output is being written to {path}, read it with read_file \
         (supports offset/limit). You will be notified when it \
         completes.",
        path = output_path.display(),
    );
    Ok(ToolOutcome {
        structured_content: Some(serde_json::to_value(BashResult {
            stdout: "",
            stderr: "",
            exit_code: None,
            truncated: false,
            full_output_path: Some(&output_path),
            task_id: Some(id),
        })?),
        content: vec![UserContent::text(wire)],
        details: ToolDetails::Bash {
            command,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            truncated: false,
            full_output_path: Some(output_path),
            stdout_truncation: None,
            stderr_truncation: None,
            task_id: Some(id),
        },
        is_error: false,
    })
}

/// Start `executed` as a background task of `ctx`'s agent, shown to the
/// model as `command`.
///
/// Nothing here awaits, so the call cannot be dropped between the spawn
/// and the driver taking the task over.
pub(super) fn launch(
    tool: &BashTool,
    ctx: &mut dyn ToolContext,
    command: String,
    executed: String,
    working_dir: PathBuf,
) -> Result<ToolOutcome, aj_agent::BoxError> {
    let spawned = match Spawned::start(tool, ctx, &command, &executed, &working_dir)? {
        Ok(spawned) => spawned,
        Err(outcome) => return Ok(outcome),
    };
    let output_path = spawned.process.output_path.clone();

    let tail = Tail::new(output_path.clone());
    let output = Arc::new(DetachedOutput {
        state: Arc::clone(&tail.state),
        path: output_path.clone(),
    });
    let kind = TaskKind::Bash {
        command: command.clone(),
    };
    let StartedTask {
        id,
        cancel,
        events,
        driver,
    } = ctx.start_background_task(kind, command.clone(), output);
    let recorded = match ctx.task_registry().record(id, spawned.state.clone()) {
        Ok(recorded) => recorded,
        Err(error) => {
            driver.abandon();
            spawned.abandon();
            return Err(format!(
                "Could not record background task #{id}, so the command was not run: {error}"
            )
            .into());
        }
    };

    let (process, watch) = spawned.go();
    let unrecorded = Unrecorded((!recorded).then(|| process.clone()));
    driver.spawn(drive(Detached {
        task_id: id,
        command: command.clone(),
        process,
        watch,
        tail,
        recorded,
        unrecorded,
        cancel,
        events,
    }));

    task_outcome("Started background task", command, id, output_path)
}

/// Owns a foreground command for as long as its tool call can be dropped.
///
/// The agent drops a cancelled call's future and never polls it again, so
/// the drop is where a cancel is acted on: it stops the command as
/// [`DetachedProcess::stop`] does.
///
/// Only the `SIGTERM` is sent from the drop itself. The escalation runs
/// on the runtime and takes the lifeline with it, so the grace is not cut
/// short. A runtime that is shutting down may never poll the escalation.
/// It drops it instead, and the lifeline with it, which kills the group.
struct Awaited {
    process: DetachedProcess,
    watch: Watch,
    /// Ends the command with this process. `None` once a drop has handed
    /// it to the escalation.
    lifeline: Option<Lifeline>,
    /// Keeps the session open through the teardown a drop hands to the
    /// runtime. `None` once the command has ended and a drop has nothing
    /// to stop.
    lease: Option<TaskCleanupGuard>,
}

impl Drop for Awaited {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        let process = self.process.clone();
        if process.alive() {
            process.signal(false);
        }
        // Without a runtime nothing can escalate. The lifeline then closes
        // with `self` and kills the group at once.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let lifeline = self.lifeline.take();
        let mut watch = std::mem::replace(&mut self.watch, Watch::Pid(Exit::default()));
        runtime.spawn(async move {
            // Held to the end: closing it earlier would cut the grace short.
            let _lifeline = lifeline;
            process.escalate().await;
            watch.reaped().await;
            // The call's result is the agent's, and names no file.
            discard_files(&process, false);
            drop(lease);
        });
    }
}

/// Run `executed` and wait for it, as the foreground call of `ctx`'s
/// agent, shown to the model as `command`.
pub(super) async fn run(
    tool: &BashTool,
    ctx: &mut dyn ToolContext,
    command: String,
    executed: String,
    working_dir: PathBuf,
    timeout_secs: u64,
) -> Result<ToolOutcome, aj_agent::BoxError> {
    let cancellation = ctx.cancellation();
    let spawned = match Spawned::start(tool, ctx, &command, &executed, &working_dir)? {
        Ok(spawned) => spawned,
        Err(outcome) => return Ok(outcome),
    };
    let lease = ctx.task_registry().track_cleanup();
    // Nothing awaits between the line and the guard, so the call cannot
    // be dropped with the command running and nothing to stop it.
    let (process, watch, lifeline) = spawned.hold();
    let mut guard = Awaited {
        process: process.clone(),
        watch,
        lifeline: Some(lifeline),
        lease: Some(lease),
    };

    let mut tail = Tail::new(process.output_path.clone());
    let timeout_at = Instant::now() + Duration::from_secs(timeout_secs);
    // `None` forces the leading-edge emit, so a cell shows the running
    // command at once.
    let mut emitted: Option<u64> = None;
    let mut pace = Pace::new();
    let end = loop {
        let more = tail.poll();
        let total = tail.total_bytes();
        let grew = emitted != Some(total);
        if grew {
            ctx.emit_update(snapshot_partial(&command, &tail.state))
                .await;
            emitted = Some(total);
        }
        let wait = if more {
            Duration::ZERO
        } else {
            pace.after(grew)
        };

        tokio::select! {
            biased;
            // Reached only when the token fires while this future is being
            // polled. Otherwise the agent drops it, and the guard acts.
            _ = cancellation.cancelled() => break ChildExit::Cancelled,
            _ = tokio::time::sleep_until(timeout_at) => break ChildExit::TimedOut,
            _ = guard.watch.ended(&process) => {
                break match process.exit_code() {
                    Some(code) => ChildExit::Exited(code),
                    None => ChildExit::Lost,
                };
            }
            _ = tokio::time::sleep(wait) => {}
        }
    };

    if matches!(end, ChildExit::Cancelled | ChildExit::TimedOut) {
        process.stop().await;
        guard.watch.reaped().await;
    }
    // The command has ended, or was stopped as far as it can be.
    guard.lease = None;
    tail.read_to_end();
    let outcome = awaited_outcome(&command, &process, &tail, end, timeout_secs)?;
    let names_output = matches!(
        &outcome.details,
        ToolDetails::Bash {
            full_output_path: Some(_),
            ..
        }
    );
    discard_files(&process, names_output);
    Ok(outcome)
}

/// Remove the files of a foreground command whose call has its result: the
/// exit file, and the output file unless the result names it.
fn discard_files(process: &DetachedProcess, keep_output: bool) {
    process.discard_exit_file();
    if !keep_output {
        let _ = std::fs::remove_file(&process.output_path);
    }
}

/// The result of a foreground call whose command ended as `end`, from the
/// output as `tail` has read it.
///
/// The result names the output file when it shows a truncated tail, since
/// the file is then the full output, and while a process the command left
/// behind may still write to it.
fn awaited_outcome(
    command: &str,
    process: &DetachedProcess,
    tail: &Tail,
    end: ChildExit,
    timeout_secs: u64,
) -> Result<ToolOutcome, aj_agent::BoxError> {
    let (output, truncation) = finalize_stream(&tail.state.lock().unwrap());
    let truncated = truncation.is_some();
    let left_behind = process.group_alive();
    let full_output_path = (truncated || left_behind).then(|| process.output_path.clone());
    let exit_code = match end {
        ChildExit::Exited(code) => Some(code),
        _ => None,
    };

    let mut wire = build_wire_content(
        &output,
        truncation.as_ref(),
        end,
        timeout_secs,
        full_output_path.as_deref(),
    );
    if left_behind {
        push_marker(
            &mut wire,
            &format!(
                "A process this command started is still running. What it writes from \
                 here on goes to {}. Use run_in_background for work that should outlive \
                 the call.",
                process.output_path.display()
            ),
        );
    }
    if let Some(error) = &tail.error {
        push_marker(&mut wire, &format!("Output capture failed: {error}"));
    }

    // A timeout, a cancel and a lost command are exceptional outcomes the
    // model should know to recover from. A non-zero exit code from a
    // command that ran to its end is a normal "the command failed" signal
    // that the wire content already conveys.
    let is_error = tail.error.is_some() || !matches!(end, ChildExit::Exited(_));
    Ok(ToolOutcome {
        structured_content: Some(serde_json::to_value(BashResult {
            stdout: &output,
            stderr: "",
            exit_code,
            truncated,
            full_output_path: full_output_path.as_deref(),
            task_id: None,
        })?),
        content: vec![UserContent::text(wire)],
        details: ToolDetails::Bash {
            command: command.to_owned(),
            stdout: output,
            stderr: String::new(),
            exit_code,
            truncated,
            full_output_path: full_output_path.clone(),
            stdout_truncation: truncation,
            stderr_truncation: None,
            task_id: None,
        },
        is_error,
    })
}

/// The result of a background launch whose call an earlier process left
/// without one: the command was recorded as task `id` and the process
/// ended before the launch was reported.
pub fn still_running_outcome(
    command: String,
    id: TaskId,
    output_path: PathBuf,
) -> Result<ToolOutcome, aj_agent::BoxError> {
    task_outcome(
        "The command is still running, as background task",
        command,
        id,
        output_path,
    )
}

/// A background bash task a [`aj_agent::TaskStore`] recorded.
#[derive(Clone, Debug)]
pub struct SavedBashTask {
    pub id: TaskId,
    pub owner: AgentId,
    /// The tool call that launched the command.
    pub call_id: String,
    pub command: String,
    /// How long ago the command was started.
    pub running_for: Duration,
    pub process: DetachedProcess,
}

/// The task notification of a recorded task whose command no longer
/// runs, for a caller that settles what an earlier process left. `None`
/// while the command runs: [`watch_background_bash`] then takes it up.
///
/// A command with an exit file has finished. One without, whose process is
/// gone, is lost: it was killed outright or the machine restarted, and its
/// status is unknown.
///
/// Reads the end of the output file, a bounded amount whatever the file
/// holds (see [`Tail::from_end`]), so it is cheap enough to call while a
/// session opens. Registers and emits nothing, so the caller saves the
/// notification and removes the record.
pub fn ended_background_bash(task: &SavedBashTask) -> Option<TaskNotice> {
    let process = &task.process;
    // Sampled before the exit file is looked for (see [`Watch::ended`]).
    let alive = process.alive();
    let end = match process.exit_code() {
        Some(code) => End::Exited(code),
        None if alive => return None,
        None => End::Lost,
    };
    let mut tail = Tail::from_end(process.output_path.clone());
    tail.read_to_end();
    let (_, notice) = end.notice(task.owner, task.id, &task.command, &tail);
    Some(notice)
}

/// Watch a task recorded by an earlier process: the agent's task registry
/// tracks it again as it tracks one started in this process. A command
/// that ended since it was last looked at is reported by its driver like
/// one that ends later. Nothing here can fail. A task the registry
/// already tracks is left alone.
///
/// Spawns the driver on the current Tokio runtime.
pub fn watch_background_bash(agent: &aj_agent::Agent, task: SavedBashTask) {
    let SavedBashTask {
        id,
        owner,
        call_id,
        command,
        running_for,
        process,
    } = task;
    let tail = Tail::from_end(process.output_path.clone());
    let output = Arc::new(DetachedOutput {
        state: Arc::clone(&tail.state),
        path: process.output_path.clone(),
    });
    let kind = TaskKind::Bash {
        command: command.clone(),
    };
    let resumed = agent.resume_background_task(
        id,
        owner,
        call_id,
        kind,
        command.clone(),
        running_for,
        output,
    );
    if let Some(StartedTask {
        id,
        cancel,
        events,
        driver,
    }) = resumed
    {
        driver.spawn(drive(Detached {
            task_id: id,
            command,
            process,
            watch: Watch::Pid(Exit::default()),
            tail,
            recorded: true,
            unrecorded: Unrecorded(None),
            cancel,
            events,
        }));
    }
}

/// How a detached command ended.
#[derive(Clone, Copy)]
enum End {
    Exited(i32),
    /// Stopped through the task registry.
    Killed,
    /// The process is gone and left no exit status.
    Lost,
}

impl End {
    /// The terminal status and the task notification, from the output as
    /// `tail` has read it.
    fn notice(
        self,
        owner: AgentId,
        task_id: TaskId,
        command: &str,
        tail: &Tail,
    ) -> (TaskStatus, TaskNotice) {
        let status = match self {
            Self::Exited(code) => TaskStatus::Exited(Some(code)),
            Self::Killed => TaskStatus::Killed,
            // The nearest status there is: the command did not exit by
            // itself as far as anyone can tell.
            Self::Lost => TaskStatus::Exited(None),
        };
        let status = background_terminal_status(status, tail.error.is_some());
        let mut body = match self {
            Self::Lost => format!(
                "Background task #{task_id} lost: {command} — status unknown. Its process \
                 is gone and left no exit status: it was killed, or the machine restarted."
            ),
            _ => format!(
                "Background task #{task_id} finished: {command} — {}",
                task_status_text(status)
            ),
        };
        let (output, truncation, skipped) = {
            let state = tail.state.lock().unwrap();
            let (output, truncation) = finalize_stream(&state);
            (output, truncation, state.skipped_bytes > 0)
        };
        let block = match &truncation {
            // The lines before the part that was read are not counted, so
            // the marker counts bytes.
            Some(truncation) if skipped => {
                let mut block = output;
                push_marker(
                    &mut block,
                    &skipped_stream_marker("stdout", truncation, &tail.path),
                );
                block
            }
            _ => render_stream_block(&output, "", truncation.as_ref(), None, Some(&tail.path)),
        };
        if !block.is_empty() {
            body.push('\n');
            body.push_str(&block);
        }
        if let Some(error) = &tail.error {
            body.push_str(&format!("\nOutput capture failed: {error}"));
        }
        if !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&format!("Full output: {}", tail.path.display()));
        let notice = TaskNotice {
            owner,
            task_id,
            kind: TaskKind::Bash {
                command: command.to_owned(),
            },
            label: command.to_owned(),
            status,
            body,
        };
        (status, notice)
    }
}

/// Everything the driver of a detached command owns.
struct Detached {
    task_id: TaskId,
    command: String,
    process: DetachedProcess,
    watch: Watch,
    tail: Tail,
    /// Whether the session recorded the task. A recorded task is left
    /// running when the registry lets go of it.
    recorded: bool,
    /// Armed for a task that is not recorded, from before the driver is
    /// spawned: a runtime can drop a spawned driver it never polled.
    unrecorded: Unrecorded,
    cancel: CancellationToken,
    events: TaskEventSink,
}

/// Kills a command nobody recorded when its driver is dropped before the
/// command ended, as when the runtime shuts down under it. Nothing else
/// could reach the command afterwards.
struct Unrecorded(Option<DetachedProcess>);

impl Drop for Unrecorded {
    fn drop(&mut self) {
        if let Some(process) = &self.0
            && process.alive()
        {
            process.signal(true);
        }
    }
}

/// Drive a detached command until it ends or the registry lets go of it:
/// announce it with `TaskStart`, emit throttled `TaskOutput` snapshots as
/// its output file grows, stop it when it is killed, and end with the
/// saved notification and `TaskEnd`.
///
/// Returning without `TaskEnd` leaves a recorded task to a later driver.
async fn drive(task: Detached) {
    let Detached {
        task_id,
        command,
        process,
        mut watch,
        mut tail,
        recorded,
        mut unrecorded,
        cancel,
        events,
    } = task;

    events
        .started(TaskKind::Bash {
            command: command.clone(),
        })
        .await;

    // `None` forces the leading-edge emit, so a cell shows the running
    // command at once.
    let mut emitted: Option<u64> = None;
    let mut pace = Pace::new();
    let end = loop {
        let more = tail.poll();
        let total = tail.total_bytes();
        // Quiet output emits nothing: an idle watcher would otherwise push
        // identical snapshots onto the bus for as long as it runs.
        let grew = emitted != Some(total);
        if grew {
            let mut partial = snapshot_partial(&command, &tail.state);
            if let ToolDetails::Bash {
                task_id: tid,
                full_output_path,
                ..
            } = &mut partial
            {
                *tid = Some(task_id);
                *full_output_path = Some(process.output_path.clone());
            }
            events.output(partial).await;
            emitted = Some(total);
        }
        let wait = if more {
            Duration::ZERO
        } else {
            pace.after(grew)
        };

        tokio::select! {
            biased;
            // The task token is the only cancellation that reaches a
            // background task. The originating turn's token is not wired
            // in: outliving the turn is the point.
            _ = cancel.cancelled() => {
                if recorded && !events.kill_requested() {
                    return;
                }
                process.stop().await;
                watch.reaped().await;
                break End::Killed;
            }
            _ = watch.ended(&process) => {
                break match process.exit_code() {
                    Some(code) => End::Exited(code),
                    None => End::Lost,
                };
            }
            _ = tokio::time::sleep(wait) => {}
        }
    };

    // The command has ended, or was stopped as far as it can be.
    unrecorded.0 = None;
    tail.read_to_end();
    let (status, notice) = end.notice(events.owner(), task_id, &command, &tail);
    if !events.finished(status, notice).await {
        // The task has ended and its record says it runs. Nothing is
        // delivered from memory, so the record stays the one account of
        // the task, and the host that next takes it up saves its end.
        return;
    }
    process.discard_exit_file();
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use aj_agent::message::AgentMessage;
    use aj_agent::{BoxError, RecordedTask, TaskStore};
    use tempfile::TempDir;

    use super::*;
    use crate::testing::DummyToolContext;
    use crate::tools::bash::BashInput;
    use aj_agent::tool::ToolDefinition;

    /// A [`TaskStore`] that keeps what it is given and can refuse a record.
    #[derive(Default)]
    struct Store {
        refuse: AtomicBool,
        seen: Mutex<Vec<DetachedProcess>>,
        finished: Mutex<Vec<TaskId>>,
    }

    impl Store {
        fn process(&self) -> DetachedProcess {
            self.seen.lock().unwrap().last().expect("a record").clone()
        }
    }

    impl TaskStore for Store {
        fn minted(&self, _id: TaskId) -> Result<(), BoxError> {
            Ok(())
        }

        fn record(&self, task: &RecordedTask) -> Result<(), BoxError> {
            let process = serde_json::from_value(task.state.clone())?;
            self.seen.lock().unwrap().push(process);
            if self.refuse.load(Ordering::SeqCst) {
                // Time for the command to run, were the wrapper not
                // waiting for this save.
                std::thread::sleep(Duration::from_millis(250));
                return Err("injected record failure".into());
            }
            Ok(())
        }

        fn finish(&self, id: TaskId, _: AgentId, _: &AgentMessage) -> Result<(), BoxError> {
            self.finished.lock().unwrap().push(id);
            Ok(())
        }
    }

    /// Kills a command's process group when the test ends, however it ends.
    struct Reaper(DetachedProcess);

    impl Drop for Reaper {
        fn drop(&mut self) {
            self.0.signal(true);
        }
    }

    async fn launch_in(
        ctx: &mut DummyToolContext,
        dir: &TempDir,
        command: &str,
    ) -> Result<ToolOutcome, BoxError> {
        execute_in(ctx, dir, command, true).await
    }

    async fn execute_in(
        ctx: &mut DummyToolContext,
        dir: &TempDir,
        command: &str,
        run_in_background: bool,
    ) -> Result<ToolOutcome, BoxError> {
        BashTool::new(false, Some(dir.path().to_path_buf()))
            .execute(
                ctx,
                BashInput {
                    command: command.to_string(),
                    timeout: 30,
                    description: "test".to_string(),
                    run_in_background,
                },
            )
            .await
    }

    /// A scratch directory under a root that lasts as long as the process.
    ///
    /// A command killed through its lifeline has its exit status written by
    /// the task that reaps it, which can happen after the test has returned.
    /// A write that lands while the directory is being removed leaves it
    /// behind, and under this root that is not residue in the temp
    /// directory itself.
    fn scratch() -> TempDir {
        static ROOT: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();
        let root =
            ROOT.get_or_init(|| TempDir::with_prefix("aj-task-lifetime-").expect("scratch root"));
        TempDir::new_in(root.path()).expect("create temp dir")
    }

    /// A command that ignores `SIGTERM`, has a child that does too, and
    /// writes its pid to `pid_file` once both are running.
    fn stubborn(pid_file: &Path) -> String {
        format!(
            "trap '' TERM; sleep 300 & echo $$ > '{}'; wait",
            pid_file.display()
        )
    }

    async fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
        for _ in 0..400 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn terminal(ctx: &DummyToolContext, id: TaskId) -> TaskStatus {
        tokio::time::timeout(
            Duration::from_secs(10),
            ctx.task_registry().wait_terminal(id),
        )
        .await
        .expect("the task ends")
        .expect("the task is known")
    }

    fn pid_in(path: &Path) -> Option<u32> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    fn running(pid: u32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        !stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
    }

    /// Both streams land in the one file in the order written, and the
    /// read and the notification report them as one stream with an exact total.
    #[tokio::test]
    async fn stdout_and_stderr_are_one_stream_in_the_order_written() {
        let dir = TempDir::new().unwrap();
        let mut ctx = DummyToolContext::default();
        launch_in(&mut ctx, &dir, "echo one; echo two >&2; echo three")
            .await
            .expect("launch");
        assert_eq!(terminal(&ctx, 1).await, TaskStatus::Exited(Some(0)));

        let (_, read) = ctx.task_registry().read(1).expect("readable");
        assert_eq!(read.stdout_tail, "one\ntwo\nthree\n");
        assert_eq!(read.stdout_total_bytes, 14);
        assert_eq!(
            (read.stderr_tail.as_str(), read.stderr_total_bytes),
            ("", 0)
        );
        let path = read.spill_path.expect("the output path");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\nthree\n");

        let notices = ctx.task_registry().drain_notices(AgentId::Main);
        let body = &notices[0].body;
        assert!(body.contains("one\ntwo\nthree\n"), "{body:?}");
        assert!(!body.contains("STDERR"), "{body:?}");
        // Nothing reads the exit file once the end is reported.
        wait_for(
            || !path.with_extension("exit").exists(),
            "the exit file to be discarded",
        )
        .await;
    }

    /// The status is the shell's, so a signal death is `128 + N`, whether
    /// the command or the wrapper itself was killed.
    #[tokio::test]
    async fn a_signal_death_ends_with_the_shells_exit_code() {
        let dir = TempDir::new().unwrap();
        let mut ctx = DummyToolContext::default();
        launch_in(&mut ctx, &dir, "kill -KILL $$")
            .await
            .expect("launch");
        assert_eq!(terminal(&ctx, 1).await, TaskStatus::Exited(Some(137)));

        // A wrapper killed outright cannot write its status. The process
        // that started it knows the status and writes it.
        let store = Arc::new(Store::default());
        ctx.task_registry().set_store(Arc::<Store>::clone(&store));
        launch_in(&mut ctx, &dir, "sleep 300")
            .await
            .expect("launch");
        let process = store.process();
        let _reaper = Reaper(process.clone());
        assert!(process.alive());
        nix::sys::signal::kill(process.pgid().unwrap(), nix::sys::signal::Signal::SIGKILL)
            .expect("kill the wrapper");
        assert_eq!(terminal(&ctx, 2).await, TaskStatus::Exited(Some(137)));
    }

    /// A kill reaches every process of the command, and the task ends as
    /// killed. The output is the command's alone: the wrapper's shell
    /// reports the terminated command on its own stderr, which is not the
    /// command's.
    #[tokio::test]
    async fn a_kill_stops_the_commands_process_group() {
        let dir = TempDir::new().unwrap();
        let pid_file = dir.path().join("child.pid");
        let mut ctx = DummyToolContext::default();
        let command = format!(
            "echo out; echo err >&2; sleep 300 & echo $! > '{}'; wait",
            pid_file.display()
        );
        launch_in(&mut ctx, &dir, &command).await.expect("launch");
        wait_for(|| pid_in(&pid_file).is_some(), "the child's pid").await;
        let child = pid_in(&pid_file).unwrap();
        assert!(running(child));

        assert!(ctx.task_registry().kill(1));
        assert_eq!(terminal(&ctx, 1).await, TaskStatus::Killed);
        wait_for(|| !running(child), "the child to be gone").await;
        let notices = ctx.task_registry().drain_notices(AgentId::Main);
        let body = &notices[0].body;
        assert!(body.contains("— killed"), "{body:?}");
        assert!(body.contains("out\nerr\n"), "{body:?}");
        assert!(!body.contains("Terminated"), "{body:?}");
    }

    /// A command that ignores `SIGTERM` is killed after the grace.
    #[tokio::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: waits out the kill grace")]
    async fn a_kill_escalates_when_sigterm_is_ignored() {
        let dir = TempDir::new().unwrap();
        let pid_file = dir.path().join("child.pid");
        let mut ctx = DummyToolContext::default();
        let command = format!(
            "trap '' TERM; sleep 300 & echo $! > '{}'; wait",
            pid_file.display()
        );
        launch_in(&mut ctx, &dir, &command).await.expect("launch");
        wait_for(|| pid_in(&pid_file).is_some(), "the child's pid").await;
        let child = pid_in(&pid_file).unwrap();

        assert!(ctx.task_registry().kill(1));
        assert_eq!(terminal(&ctx, 1).await, TaskStatus::Killed);
        wait_for(|| !running(child), "the child to be gone").await;
    }

    /// The task ends when its command does. A process the command left
    /// behind neither holds the task open nor is stopped.
    #[tokio::test]
    async fn a_process_left_behind_does_not_hold_the_task_open() {
        let dir = TempDir::new().unwrap();
        let pid_file = dir.path().join("child.pid");
        let store = Arc::new(Store::default());
        let mut ctx = DummyToolContext::default();
        ctx.task_registry().set_store(Arc::<Store>::clone(&store));
        let command = format!("sleep 300 & echo $! > '{}'; echo done", pid_file.display());
        launch_in(&mut ctx, &dir, &command).await.expect("launch");
        let _reaper = Reaper(store.process());
        assert_eq!(terminal(&ctx, 1).await, TaskStatus::Exited(Some(0)));
        let child = pid_in(&pid_file).expect("the child's pid");
        assert!(running(child), "the straggler is left alone");
        let notices = ctx.task_registry().drain_notices(AgentId::Main);
        assert!(notices[0].body.contains("done"), "{:?}", notices[0].body);
    }

    /// A launch that cannot be recorded fails, and its command never runs:
    /// the wrapper is still waiting for its line when the record is refused.
    #[tokio::test]
    async fn a_refused_record_fails_the_launch_and_runs_nothing() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("ran");
        let store = Arc::new(Store::default());
        store.refuse.store(true, Ordering::SeqCst);
        let mut ctx = DummyToolContext::default();
        ctx.task_registry().set_store(Arc::<Store>::clone(&store));

        let command = format!("touch '{}'; sleep 300", marker.display());
        let error = launch_in(&mut ctx, &dir, &command)
            .await
            .expect_err("the launch fails");
        assert!(error.to_string().contains("was not run"), "{error}");

        let process = store.process();
        let _reaper = Reaper(process.clone());
        wait_for(|| !running(process.pid), "the wrapper to be gone").await;
        assert!(!marker.exists(), "the command ran unrecorded");
        wait_for(|| !process.group_alive(), "the wrapper to be reaped").await;
        assert_eq!(ctx.task_registry().status(1), None);
        assert!(ctx.task_registry().snapshot().is_empty());
        assert!(!process.output_path.exists() && !process.exit_path.exists());
    }

    /// A foreground command costs its session nothing durable: no record,
    /// no task, and no file once its result is built. One that ran to its
    /// end is not reported as having left a process behind, which the
    /// wrapper's own lifeline watcher would be if the wrapper did not wait
    /// for it. Repeated, because that report is a race when it is wrong.
    #[tokio::test]
    async fn a_foreground_command_leaves_no_record_no_file_and_no_process() {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(Store::default());
        let mut ctx = DummyToolContext::default();
        ctx.task_registry().set_store(Arc::<Store>::clone(&store));
        for _ in 0..25 {
            let outcome = execute_in(&mut ctx, &dir, "echo out; exit 3", false)
                .await
                .expect("execute");
            let ToolDetails::Bash {
                stdout,
                exit_code,
                task_id,
                full_output_path,
                ..
            } = &outcome.details
            else {
                panic!("bash details: {:?}", outcome.details);
            };
            assert_eq!((stdout.as_str(), *exit_code), ("out\n", Some(3)));
            assert_eq!((*task_id, full_output_path), (None, &None));
            let text = format!("{:?}", outcome.content);
            assert!(!text.contains("still running"), "{text}");
        }
        assert!(store.seen.lock().unwrap().is_empty(), "nothing is recorded");
        assert!(ctx.task_registry().snapshot().is_empty(), "no task");
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert!(left.is_empty(), "files left behind: {left:?}");
    }

    /// A foreground command dies with the process that started it, by the
    /// lifeline alone. The wrapper is spawned as [`run`] spawns it, and
    /// then only the pipe is closed, which is all the system does for a
    /// process that died: no signal is sent and no teardown runs.
    ///
    /// The command ignores `SIGTERM` and has been sent one, as a host that
    /// died in the middle of a polite stop leaves it.
    #[tokio::test]
    async fn a_foreground_command_dies_when_its_lifeline_closes() {
        let dir = scratch();
        let pid_file = dir.path().join("pid");
        let tool = BashTool::new(false, Some(dir.path().join("output")));
        let ctx = DummyToolContext::default();
        let command = stubborn(&pid_file);
        let spawned = Spawned::start(&tool, &ctx, &command, &command, dir.path())
            .expect("start")
            .unwrap_or_else(|_| panic!("the wrapper spawns"));
        let (process, _watch, lifeline) = spawned.hold();
        let _reaper = Reaper(process.clone());
        wait_for(|| pid_in(&pid_file).is_some(), "the command to run").await;

        process.signal(false);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            running(pid_in(&pid_file).unwrap()),
            "the command shrugs off the SIGTERM, otherwise the signal ended it"
        );

        drop(lifeline);
        wait_for(|| !process.group_alive(), "the process group to be gone").await;
    }

    /// The wrapper runs its command only once it is told to. A process that
    /// dies between the spawn and the record closes the pipe without a
    /// word, and then nothing runs and no status is written.
    #[tokio::test]
    async fn the_wrapper_runs_nothing_until_it_is_told_to() {
        let dir = TempDir::new().unwrap();
        let (marker, exit) = (dir.path().join("ran"), dir.path().join("exit"));
        let wrapper = || {
            let mut cmd = Command::new("bash");
            cmd.arg("-c")
                .arg(WRAPPER)
                .arg("aj-task")
                .arg(format!("touch '{}'; exit 3", marker.display()))
                .arg(&exit)
                .stdin(Stdio::piped());
            cmd.spawn().expect("spawn the wrapper")
        };

        let mut untold = wrapper();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(!marker.exists(), "the command ran before the line");
        drop(untold.stdin.take());
        assert_eq!(untold.wait().await.unwrap().code(), Some(125));
        assert!(!marker.exists() && !exit.exists());

        let mut mistold = wrapper();
        let mut gate = mistold.stdin.take().unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut gate, b"start\n")
            .await
            .unwrap();
        assert_eq!(mistold.wait().await.unwrap().code(), Some(125));
        assert!(!marker.exists() && !exit.exists());

        let mut told = wrapper();
        let mut gate = told.stdin.take().unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut gate, b"go\n")
            .await
            .unwrap();
        assert_eq!(told.wait().await.unwrap().code(), Some(3));
        assert!(marker.exists());
        assert_eq!(std::fs::read_to_string(&exit).unwrap(), "3\n");
        assert!(!exit_tmp_path(&exit).exists());
    }

    /// A recorded task belongs to its record: the registry shutting down
    /// lets go of it and the command keeps running. One that nobody
    /// recorded ends with its registry.
    #[tokio::test]
    async fn only_a_recorded_task_outlives_its_registry() {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(Store::default());
        let mut recorded = DummyToolContext::default();
        recorded
            .task_registry()
            .set_store(Arc::<Store>::clone(&store));
        launch_in(&mut recorded, &dir, "sleep 300")
            .await
            .expect("launch");
        let process = store.process();
        let _reaper = Reaper(process.clone());
        let registry = recorded.task_registry();
        registry.shutdown();
        assert!(registry.quiesce(Duration::from_secs(5)).await);
        assert_eq!(registry.status(1), None, "the registry let go of it");
        assert!(!registry.has_notices(AgentId::Main));
        assert!(store.finished.lock().unwrap().is_empty());
        assert!(process.alive(), "the command keeps running");

        let pid_file = dir.path().join("child.pid");
        let mut unrecorded = DummyToolContext::default();
        let command = format!("echo $$ > '{}'; sleep 300", pid_file.display());
        launch_in(&mut unrecorded, &dir, &command)
            .await
            .expect("launch");
        wait_for(|| pid_in(&pid_file).is_some(), "the command's pid").await;
        let pid = pid_in(&pid_file).unwrap();
        let registry = unrecorded.task_registry();
        registry.shutdown();
        assert!(registry.quiesce(Duration::from_secs(5)).await);
        assert_eq!(registry.status(1), Some(TaskStatus::Killed));
        wait_for(|| !running(pid), "the command to be gone").await;
    }

    /// A quiet command's driver looks at the output file once per
    /// [`QUIET_POLL`], and output that resumes after a long quiet spell is
    /// emitted within that. Time is paused, so the count is exact.
    #[tokio::test(start_paused = true)]
    async fn a_quiet_task_is_looked_at_rarely_and_resumed_output_arrives_within_the_cap() {
        use std::sync::atomic::Ordering::Relaxed;

        let dir = TempDir::new().unwrap();
        let output_path = dir.path().join("out.log");
        std::fs::write(&output_path, "early\n").unwrap();
        let mut ctx = DummyToolContext::default();
        let seen: Arc<Mutex<Vec<(Instant, String)>>> = Arc::default();
        let sink = Arc::clone(&seen);
        let _sub = ctx
            .bus
            .subscribe(aj_agent::bus::listener_from_sync(move |event| {
                if let aj_agent::events::AgentEvent::TaskOutput {
                    partial: ToolDetails::Bash { stdout, .. },
                    ..
                } = event
                {
                    sink.lock().unwrap().push((Instant::now(), stdout.clone()));
                }
            }));

        let tail = Tail::new(output_path.clone());
        let polls = Arc::clone(&tail.polls);
        let output = Arc::new(DetachedOutput {
            state: Arc::clone(&tail.state),
            path: output_path.clone(),
        });
        let kind = TaskKind::Bash {
            command: "quiet".to_string(),
        };
        let StartedTask {
            id,
            cancel,
            events,
            driver,
        } = ctx.start_background_task(kind, "quiet".to_string(), output);
        // The sender is held, so the command never ends. No process is
        // looked at before that.
        let (_running, reaped) = tokio::sync::oneshot::channel();
        driver.spawn(drive(Detached {
            task_id: id,
            command: "quiet".to_string(),
            process: DetachedProcess {
                pid: 0,
                start_time: 0,
                output_path: output_path.clone(),
                exit_path: dir.path().join("out.exit"),
            },
            watch: Watch::Child(reaped),
            tail,
            recorded: true,
            unrecorded: Unrecorded(None),
            cancel,
            events,
        }));

        let quiet = Duration::from_secs(600);
        tokio::time::sleep(quiet).await;
        let looks = polls.load(Relaxed);
        // One look per `QUIET_POLL`, and a few more while the wait grows.
        let steady = u64::try_from(quiet.as_millis() / QUIET_POLL.as_millis()).unwrap();
        assert!(
            (steady / 2..=steady + 8).contains(&looks),
            "{looks} looks in {quiet:?}"
        );
        assert_eq!(seen.lock().unwrap().len(), 1, "quiet output emits nothing");

        let written = Instant::now();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&output_path)
            .unwrap()
            .write_all(b"late\n")
            .unwrap();
        tokio::time::sleep(QUIET_POLL * 2).await;
        let (at, stdout) = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(stdout, "early\nlate\n");
        assert!(at - written <= QUIET_POLL, "{:?}", at - written);
        // Output that flows is read at the debounce rate again.
        let before = polls.load(Relaxed);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&output_path)
            .unwrap()
            .write_all(b"more\n")
            .unwrap();
        tokio::time::sleep(UPDATE_DEBOUNCE * 2).await;
        assert!(polls.load(Relaxed) > before);
    }

    /// The end of a wrapper another process started is waited for on a
    /// descriptor, not looked for.
    #[tokio::test]
    async fn the_end_of_a_wrapper_this_process_did_not_start_is_not_polled() {
        let dir = scratch();
        let tool = BashTool::new(false, Some(dir.path().join("output")));
        let ctx = DummyToolContext::default();
        let spawned = Spawned::start(&tool, &ctx, "sleep 300", "sleep 300", dir.path())
            .expect("start")
            .unwrap_or_else(|_| panic!("the wrapper spawns"));
        let (process, _reaped) = spawned.go();
        let _reaper = Reaper(process.clone());

        let mut watch = Watch::Pid(Exit::default());
        let pending = tokio::time::timeout(Duration::from_millis(200), watch.ended(&process));
        assert!(pending.await.is_err(), "the command runs");
        assert!(matches!(watch, Watch::Pid(Exit::Fd(_))));

        process.signal(true);
        tokio::time::timeout(Duration::from_secs(10), watch.ended(&process))
            .await
            .expect("the end is seen");
        assert!(!process.alive());
        // A wrapper that is gone ends the wait at once, however it is asked.
        let mut late = Watch::Pid(Exit::default());
        tokio::time::timeout(Duration::from_secs(10), late.ended(&process))
            .await
            .expect("a gone wrapper has ended");
    }

    /// Taking up a task costs the same whatever its command has written.
    /// The file is sparse and far larger than could be read in the time
    /// allowed, so only a read from its end passes. The tail is the same
    /// lines a read of the whole file would end with, the total is the
    /// file's length, and the marker counts bytes, since the lines before
    /// the part that was read are unknown.
    #[test]
    fn a_task_with_a_huge_output_file_is_taken_up_by_its_end() {
        const LINE: usize = "line 0000000\n".len();
        let dir = TempDir::new().unwrap();
        let output_path = dir.path().join("task.log");
        let exit_path = dir.path().join("task.exit");
        let hole: u64 = 64 << 30;
        let lines = 3 * ROLLING_CAP_BYTES / LINE;
        let written: String = (0..lines).map(|n| format!("line {n:07}\n")).collect();
        {
            let mut file = std::fs::File::create(&output_path).unwrap();
            file.set_len(hole).unwrap();
            file.seek(SeekFrom::End(0)).unwrap();
            file.write_all(written.as_bytes()).unwrap();
        }
        std::fs::write(&exit_path, "3\n").unwrap();
        let total = hole + u64::try_from(written.len()).unwrap();
        assert_ne!(
            ROLLING_CAP_BYTES % LINE,
            0,
            "the part that is read starts inside a line"
        );
        let task = SavedBashTask {
            id: 7,
            owner: AgentId::Main,
            call_id: "call".to_string(),
            command: "serve".to_string(),
            running_for: Duration::ZERO,
            // No process has this pid: the command has ended.
            process: DetachedProcess {
                pid: u32::MAX,
                start_time: 0,
                output_path: output_path.clone(),
                exit_path,
            },
        };

        let started = std::time::Instant::now();
        let notice = ended_background_bash(&task).expect("the command has ended");
        let mut tail = Tail::from_end(output_path.clone());
        tail.read_to_end();
        let (read, read_total) = tail_snapshot(&tail.state);
        let took = started.elapsed();
        assert!(took < Duration::from_secs(5), "took {took:?}");

        // What `task_output` reports of a task that still runs: whole lines
        // up to the last, and every byte counted.
        assert_eq!(read_total, total);
        let whole_lines = ROLLING_CAP_BYTES / LINE;
        assert_eq!(read, written[written.len() - whole_lines * LINE..]);

        // The notification shows the last lines the display budget allows.
        let shown = crate::truncate::BASH_MAX_LINES;
        let expected = &written[written.len() - shown * LINE..];
        let first_shown = lines - shown;
        assert!(
            notice.body.contains(&format!(
                "exit code 3\n{expected}[Showing last {} of {} of stdout. Full output at {}]\n",
                crate::truncate::format_size(shown * LINE),
                crate::truncate::format_size(usize::try_from(total).unwrap()),
                output_path.display(),
            )),
            "{}",
            &notice.body[..200]
        );
        assert!(expected.starts_with(&format!("line {first_shown:07}\n")));
        assert!(
            !notice.body.contains("Showing lines"),
            "a line-count marker"
        );
    }

    /// A pid is the wrapper only together with its start time.
    #[test]
    fn a_pid_with_another_start_time_is_not_the_task() {
        use std::os::unix::process::CommandExt;

        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .process_group(0)
            .spawn()
            .expect("spawn");
        let pid = child.id();
        let start_time = process_start_time(pid).expect("a running group leader");
        let process = |start_time| DetachedProcess {
            pid,
            start_time,
            output_path: PathBuf::new(),
            exit_path: PathBuf::new(),
        };
        assert!(process(start_time).alive());
        assert!(!process(start_time + 1).alive());
        // One that leads no group is no wrapper.
        assert_eq!(process_start_time(std::process::id()), None);
        child.kill().expect("kill");
        // Exited and not reaped is not running.
        let exited = std::time::Instant::now();
        while process(start_time).alive() {
            assert!(exited.elapsed() < Duration::from_secs(10), "still running");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
        child.wait().expect("reap");
        assert!(!process(start_time).alive());
    }
}
