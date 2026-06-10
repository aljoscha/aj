//! Terminal abstraction layer.
//!
//! Provides a [`Terminal`] trait and a [`ProcessTerminal`] implementation
//! that drives a real terminal over stdin/stdout. Headless test doubles
//! live alongside the integration tests under `tests/support/`.
//!
//! # Input pipeline
//!
//! Terminals expose their input source as a [`Stream`] via
//! [`Terminal::take_input_stream`]. The [`crate::tui::Tui`] event loop
//! polls this stream inside its `tokio::select!` alongside the render
//! throttle. Crossterm parses CSI / OSC / DCS / APC sequences,
//! bracketed paste, mouse reports, and the Kitty keyboard protocol at
//! the `ProcessTerminal` boundary; events reach the rest of the crate
//! as already typed [`InputEvent`]s.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{self, disable_raw_mode, enable_raw_mode};
use crossterm::{execute, queue};
use futures::stream::{Stream, StreamExt};

use crate::keys::InputEvent;

/// Boxed, pinned stream of [`InputEvent`]s returned by
/// [`Terminal::take_input_stream`]. Must be `Send + 'static` so the
/// async event loop can poll it from any task.
pub type InputStream = Pin<Box<dyn Stream<Item = InputEvent> + Send + 'static>>;

/// Environment variable consulted by [`ProcessTerminal::new`] to enable
/// a write log. When set, every byte sequence written through
/// `Terminal::write` on a [`ProcessTerminal`] is also appended to a
/// file on disk — useful for replaying real-terminal rendering bugs
/// into the in-memory `VirtualTerminal` to bisect the offending frame.
///
/// The value may be either a file path (the log appends directly to
/// that path) or an existing directory (a per-process log named
/// `aj-tui-<unix-seconds>-<pid>.log` is created inside it). The
/// distinction is made at construction time by stat'ing the value;
/// paths that don't exist yet are treated as file paths.
pub const WRITE_LOG_ENV: &str = "AJ_TUI_WRITE_LOG";

/// OSC 9;4 escape sequence emitted to start an indeterminate progress
/// indicator on the terminal taskbar / window badge. The state code
/// `3` means "indeterminate" (no value parameter); see the OSC 9;4
/// progress protocol (ConEmu / Windows Terminal).
const TERMINAL_PROGRESS_ACTIVE_SEQUENCE: &str = "\x1b]9;4;3\x07";

/// OSC 9;4 escape sequence emitted to clear the progress indicator.
/// State code `0` clears; the trailing `;` is harmless on terminals
/// that implement the protocol and is ignored on ones that don't.
const TERMINAL_PROGRESS_CLEAR_SEQUENCE: &str = "\x1b]9;4;0;\x07";

/// Cadence at which an active progress indicator is re-emitted while
/// it remains active. Some terminals (notably Windows Terminal) clear
/// a stale indeterminate indicator after a timeout if the application
/// stops sending updates; the keepalive prevents that.
const TERMINAL_PROGRESS_KEEPALIVE: Duration = Duration::from_millis(1000);

/// How often the keepalive thread checks the cancellation flag while
/// sleeping between emissions. A finer-grained poll keeps `set_progress(false)`
/// (and `Drop`) responsive — without it, the worst-case wait for a
/// thread to notice a cancel is one full keepalive interval.
const TERMINAL_PROGRESS_CANCEL_POLL: Duration = Duration::from_millis(50);

/// Global state tracking for terminal restoration from the panic hook.
///
/// These are read from the panic hook (which has no access to any
/// `ProcessTerminal` instance), so they have to live in statics.
static RAW_MODE_ACTIVE: AtomicBool = AtomicBool::new(false);
static BRACKETED_PASTE_ACTIVE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_ENHANCEMENT_ACTIVE: AtomicBool = AtomicBool::new(false);
static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK_INSTALLED: Once = Once::new();

/// Restore the terminal to a sane state. Safe to call multiple times; only
/// undoes state that is currently marked active via the atomics above.
fn restore_terminal_state() {
    let mut stdout = io::stdout();
    if CURSOR_HIDDEN.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout, crossterm::cursor::Show);
    }
    if BRACKETED_PASTE_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout, DisableBracketedPaste);
    }
    if KEYBOARD_ENHANCEMENT_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    if RAW_MODE_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = disable_raw_mode();
    }
    let _ = stdout.flush();
}

/// Install a panic hook (once) that restores the terminal before the
/// previously-registered hook runs. Without this, a panic while in raw mode
/// leaves the user's shell unusable and prints the panic message with no
/// line-ending translation.
fn install_panic_hook() {
    PANIC_HOOK_INSTALLED.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal_state();
            prev(info);
        }));
    });
}

/// Abstraction over a terminal for input/output.
pub trait Terminal {
    /// Write a string to the terminal output.
    fn write(&mut self, data: &str);

    /// Bring the terminal into the state required for interactive rendering
    /// (raw mode, bracketed paste, keyboard enhancement, an input reader,
    /// etc.). Implementations must make this idempotent. The default is a
    /// no-op, which is appropriate for in-memory terminals used in tests.
    fn start(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Tear down any terminal-wide state (raw mode, bracketed paste, keyboard
    /// enhancement flags, etc.) that this terminal set up. Implementations
    /// should make this idempotent. The default implementation is a no-op,
    /// which is appropriate for in-memory terminals used in tests.
    fn stop(&mut self) {}

    /// Get the current terminal width in columns.
    fn columns(&self) -> u16;

    /// Get the current terminal height in rows.
    fn rows(&self) -> u16;

    /// Move cursor up (negative) or down (positive) by `lines`.
    fn move_by(&mut self, lines: i32);

    /// Hide the cursor.
    fn hide_cursor(&mut self);

    /// Show the cursor.
    fn show_cursor(&mut self);

    /// Clear the entire current line, regardless of cursor column.
    ///
    /// Implementations must emit CSI `2K` (erase whole line) rather than
    /// CSI `K` (erase from cursor to end of line). The render engine
    /// inlines `\x1b[2K` in its diffing path (see `tui.rs`) and this trait
    /// method must behave identically so the two surfaces are
    /// interchangeable for component authors. Clearing only from the
    /// cursor would leave stale bytes to the left of the cursor on any
    /// redraw that didn't happen to start at column 0.
    fn clear_line(&mut self);

    /// Clear from cursor to end of screen.
    fn clear_from_cursor(&mut self);

    /// Clear the entire screen and move cursor to home position.
    fn clear_screen(&mut self);

    /// Set the terminal window title.
    fn set_title(&mut self, title: &str);

    /// Set the indeterminate-progress indicator on the terminal taskbar /
    /// window badge.
    ///
    /// Passing `true` signals "work in progress" via the OSC 9;4;3
    /// indeterminate state (the ConEmu / Windows Terminal progress
    /// protocol). Passing `false` clears it (OSC 9;4;0;).
    ///
    /// Implementations that participate in the protocol typically also
    /// re-emit the active sequence on a timer while the indicator
    /// remains on; the trait only guarantees the on/off transitions.
    ///
    /// The default implementation is a no-op, which is appropriate for
    /// in-memory terminals used in tests. Tests that want to assert on
    /// the emitted escape sequence can still read the writes log on
    /// `VirtualTerminal`, which also captures the logical active flag
    /// in its own field.
    fn set_progress(&mut self, active: bool) {
        let _ = active;
    }

    /// Flush the output buffer.
    fn flush(&mut self);

    /// Take ownership of the terminal's input stream. Called once by
    /// [`crate::tui::Tui::start`] to wire the terminal's input source
    /// into the TUI's async event loop.
    ///
    /// The default returns `None`, meaning "this terminal has no input
    /// source; the `Tui` will only surface [`crate::tui::TuiEvent::Render`]
    /// events." Real-terminal implementations return a stream of
    /// [`InputEvent`]s; test doubles hand out a channel-backed stream
    /// the test drives synthetic input through.
    ///
    /// May be called multiple times; subsequent calls return `None`
    /// (the stream is moved out, not cloned).
    fn take_input_stream(&mut self) -> Option<InputStream> {
        None
    }

    /// Per-cell pixel dimensions, when the host terminal reports them.
    ///
    /// Used by image-protocol encoders to scale images to a cell-grid
    /// footprint that lines up with surrounding text. Returns `None`
    /// when the terminal doesn't expose pixel sizes — image renderers
    /// then fall back to a hard-coded default.
    ///
    /// Read once at component construction; mid-session font-size
    /// changes are not handled today. TODO: re-probe on resize when
    /// terminals start reporting pixel sizes through SIGWINCH paths.
    fn cell_pixel_size(&self) -> Option<(u32, u32)> {
        None
    }

    /// Whether the Kitty keyboard protocol is currently active on this
    /// terminal. Components can branch on this to know whether they will
    /// see key-release events and disambiguated modifier encodings.
    ///
    /// Implementations that do not participate in the negotiation (the
    /// default) return `false`. `ProcessTerminal` returns `true` once it
    /// has successfully pushed the Kitty enhancement flags; headless test
    /// terminals may hard-code this to `true` so components under test see
    /// the same encoding path as the real terminal.
    fn kitty_protocol_active(&self) -> bool {
        false
    }
}

/// Resolve the [`WRITE_LOG_ENV`] environment variable into a concrete
/// file path, or `None` if the variable is unset or empty.
///
/// If the value points at an existing directory, the resolved path is
/// `<dir>/aj-tui-<unix-seconds>-<pid>.log` so multiple processes
/// writing into the same directory get distinct files. Otherwise the
/// value is used verbatim as a file path (the parent directory is not
/// created; callers that want a fresh directory should create it
/// themselves).
fn resolve_write_log_path() -> Option<PathBuf> {
    let value = std::env::var(WRITE_LOG_ENV).ok()?;
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(&value);
    if path.is_dir() {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let filename = format!("aj-tui-{}-{}.log", ts, process::id());
        Some(path.join(filename))
    } else {
        Some(path)
    }
}

/// Resolve a terminal dimension (columns or rows) through the
/// standard fallback chain:
///
/// 1. If the OS reported a live size (`probe = Some(_)`), use it.
///    Typically this comes from `ioctl(TIOCGWINSZ)` on the stdout
///    file descriptor via [`crossterm::terminal::size`].
/// 2. Otherwise consult `env_var` (`COLUMNS` or `LINES`); a parseable
///    `u16` value wins.
/// 3. Otherwise fall back to `default` (80 cols / 24 rows).
///
/// Step 2 matters when stdout isn't a TTY — typical under shell
/// pipelines, CI, and inside the cargo test harness — and lets users
/// hint a sensible width without an attached terminal.
fn resolve_dimension(probe: Option<u16>, env_var: &str, default: u16) -> u16 {
    if let Some(v) = probe {
        return v;
    }
    std::env::var(env_var)
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(default)
}

/// Live size of the stdout terminal, or `None` if stdout isn't a TTY.
///
/// We deliberately gate on [`IsTerminal`] before calling
/// [`crossterm::terminal::size`]: crossterm probes stdout, then stdin,
/// then `/dev/tty`, so it can return a size even when *stdout* itself
/// is a pipe or file. Tying the probe strictly to stdout (returning
/// `None` when stdout isn't a TTY) is what makes the env-var fallback
/// in [`resolve_dimension`] kick in for non-TTY stdout.
fn stdout_size() -> Option<(u16, u16)> {
    if !io::stdout().is_terminal() {
        return None;
    }
    terminal::size().ok()
}

/// Append `data` to the write-log file at `path`. Errors are swallowed:
/// a broken logging path must not take down the TUI itself. Opens the
/// file in append mode per call so multiple instances sharing the same
/// path stay interleaved-but-intact.
fn append_write_log(path: &Path, data: &str) {
    let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = file.write_all(data.as_bytes());
}

/// Body of the progress-keepalive thread: re-emit
/// [`TERMINAL_PROGRESS_ACTIVE_SEQUENCE`] every
/// [`TERMINAL_PROGRESS_KEEPALIVE`] interval until `stop` is raised.
///
/// Polls the cancellation flag every [`TERMINAL_PROGRESS_CANCEL_POLL`]
/// so the worst-case shutdown latency is one poll interval rather
/// than one keepalive interval. Writes go straight to stdout (the
/// thread doesn't own a [`ProcessTerminal`]); when a write-log path
/// is supplied, every emission is also appended there so tests can
/// observe the keepalive cadence.
fn progress_keepalive_loop(stop: Arc<AtomicBool>, log_path: Option<PathBuf>) {
    while !stop.load(Ordering::SeqCst) {
        // Sleep up to one full keepalive interval, broken into small
        // poll windows so the thread notices a cancellation quickly.
        let mut elapsed = Duration::ZERO;
        while elapsed < TERMINAL_PROGRESS_KEEPALIVE {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let remaining = TERMINAL_PROGRESS_KEEPALIVE - elapsed;
            let sleep = remaining.min(TERMINAL_PROGRESS_CANCEL_POLL);
            thread::sleep(sleep);
            elapsed += sleep;
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        // Match `ProcessTerminal::write`: best-effort stdout write +
        // optional disk append. Swallow errors — there's nothing we
        // can usefully do from a background thread, and a transient
        // EPIPE shouldn't take the keepalive down.
        let mut stdout = io::stdout();
        let _ = stdout.write_all(TERMINAL_PROGRESS_ACTIVE_SEQUENCE.as_bytes());
        let _ = stdout.flush();
        if let Some(path) = log_path.as_deref() {
            append_write_log(path, TERMINAL_PROGRESS_ACTIVE_SEQUENCE);
        }
    }
}

/// A real terminal connected to stdin/stdout.
pub struct ProcessTerminal {
    /// Whether `start()` has run and `stop()` has not yet been called.
    started: bool,
    /// Crossterm-backed input stream waiting to be handed to the
    /// [`crate::tui::Tui`] event loop. `None` until the first
    /// successful [`Terminal::start`] creates it, then `Some` until
    /// [`ProcessTerminal::take_input_stream`] moves it out.
    input_stream: Option<InputStream>,
    /// Whether the input stream has ever been created. Keeps a
    /// stop/start cycle from building a second stream after the first
    /// one was taken.
    input_stream_created: bool,
    /// Destination for the optional [`WRITE_LOG_ENV`] write-log. `None`
    /// means the env var was unset, so writes bypass the disk logger
    /// entirely.
    write_log_path: Option<PathBuf>,
    /// Cancellation flag for the optional progress-keepalive thread.
    /// Cleared by [`ProcessTerminal::set_progress`] (`true` branch) and
    /// set by the matching `false` branch and by [`Drop`] / [`Self::stop`].
    progress_stop: Arc<AtomicBool>,
    /// Handle to the running progress-keepalive thread, if any. The
    /// thread re-emits [`TERMINAL_PROGRESS_ACTIVE_SEQUENCE`] every
    /// [`TERMINAL_PROGRESS_KEEPALIVE`] until the cancellation flag is
    /// raised.
    progress_thread: Option<thread::JoinHandle<()>>,
}

impl ProcessTerminal {
    pub fn new() -> Self {
        // Construction must not touch the process's terminal: building
        // a crossterm `EventStream` here would panic in environments
        // without any usable TTY (crossterm's "reader source not set").
        // The input stream is created in `start()`, once raw mode has
        // proven a TTY exists.
        Self {
            started: false,
            input_stream: None,
            input_stream_created: false,
            write_log_path: resolve_write_log_path(),
            progress_stop: Arc::new(AtomicBool::new(false)),
            progress_thread: None,
        }
    }

    /// Path the write-log is currently being appended to, if any. Read
    /// by tests; never surface this to end-user-facing API.
    pub fn write_log_path(&self) -> Option<&Path> {
        self.write_log_path.as_deref()
    }

    /// Stop the progress-keepalive thread if one is running. Returns
    /// `true` if a thread was joined (and therefore the indicator is
    /// no longer being re-emitted), `false` if no thread was active.
    /// Idempotent.
    fn stop_progress_thread(&mut self) -> bool {
        if let Some(handle) = self.progress_thread.take() {
            self.progress_stop.store(true, Ordering::SeqCst);
            // `join` failure means the worker panicked; we just want
            // it gone, so swallow the error and treat the thread as
            // stopped.
            let _ = handle.join();
            true
        } else {
            false
        }
    }
}

impl Default for ProcessTerminal {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ProcessTerminal {
    fn drop(&mut self) {
        // Safety net: stop the keepalive thread (if any) and restore
        // terminal state even if the caller forgot to call `stop()`
        // (or `Tui::stop()`), or if a panic unwound past `stop()`
        // without going through the panic hook.
        self.stop();
    }
}

impl Terminal for ProcessTerminal {
    fn write(&mut self, data: &str) {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(data.as_bytes());
        if let Some(path) = self.write_log_path.as_deref() {
            append_write_log(path, data);
        }
    }

    /// Enter raw mode, enable bracketed paste, and set up terminal modes.
    ///
    /// Also installs (once, process-wide) a panic hook that restores the
    /// terminal if a panic unwinds through this stack — without it, a panic
    /// while in raw mode leaves the shell unusable.
    ///
    /// Input is surfaced asynchronously via [`Self::take_input_stream`];
    /// `start` no longer spawns a blocking reader thread.
    fn start(&mut self) -> io::Result<()> {
        if self.started {
            return Ok(());
        }

        install_panic_hook();

        enable_raw_mode()?;
        RAW_MODE_ACTIVE.store(true, Ordering::SeqCst);

        // Raw mode succeeding proves a usable TTY exists, so building
        // the crossterm `EventStream` is safe now (it panics when no
        // event source is available). `Tui::start` calls
        // `take_input_stream` only after this method returns, so the
        // stream is always ready in time.
        if !self.input_stream_created {
            let events = crossterm::event::EventStream::new()
                .filter_map(|ev| async move { ev.ok().and_then(|e| InputEvent::try_from(e).ok()) });
            self.input_stream = Some(Box::pin(events));
            self.input_stream_created = true;
        }

        let mut stdout = io::stdout();
        execute!(stdout, EnableBracketedPaste)?;
        BRACKETED_PASTE_ACTIVE.store(true, Ordering::SeqCst);

        // Try to enable Kitty keyboard protocol (progressive enhancement).
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES;
        if execute!(stdout, PushKeyboardEnhancementFlags(flags)).is_ok() {
            KEYBOARD_ENHANCEMENT_ACTIVE.store(true, Ordering::SeqCst);
        }

        execute!(stdout, crossterm::cursor::Hide)?;
        CURSOR_HIDDEN.store(true, Ordering::SeqCst);

        self.started = true;
        Ok(())
    }

    /// Restore the terminal to a sane state. Idempotent.
    fn stop(&mut self) {
        // If a progress-keepalive thread is running, halt it first
        // and emit the clear sequence so the indicator goes away
        // before we hand the terminal back to the user's shell.
        if self.stop_progress_thread() {
            self.write(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
        }

        restore_terminal_state();
        self.started = false;
    }

    fn columns(&self) -> u16 {
        resolve_dimension(stdout_size().map(|(w, _)| w), "COLUMNS", 80)
    }

    fn rows(&self) -> u16 {
        resolve_dimension(stdout_size().map(|(_, h)| h), "LINES", 24)
    }

    fn move_by(&mut self, lines: i32) {
        let mut stdout = io::stdout();
        if lines < 0 {
            let n = u16::try_from(-lines).unwrap_or(u16::MAX);
            let _ = queue!(stdout, crossterm::cursor::MoveUp(n));
        } else if lines > 0 {
            let n = u16::try_from(lines).unwrap_or(u16::MAX);
            let _ = queue!(stdout, crossterm::cursor::MoveDown(n));
        }
    }

    fn hide_cursor(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, crossterm::cursor::Hide);
    }

    fn show_cursor(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, crossterm::cursor::Show);
    }

    fn clear_line(&mut self) {
        self.write("\x1b[2K");
    }

    fn clear_from_cursor(&mut self) {
        self.write("\x1b[J");
    }

    fn clear_screen(&mut self) {
        self.write("\x1b[2J\x1b[H");
    }

    fn set_title(&mut self, title: &str) {
        self.write(&format!("\x1b]0;{}\x07", title));
    }

    fn set_progress(&mut self, active: bool) {
        if active {
            // Emit the active sequence immediately so the indicator
            // shows up on the next paint.
            self.write(TERMINAL_PROGRESS_ACTIVE_SEQUENCE);

            // If a keepalive thread is already running, leave it
            // alone — re-spawning would race the existing one.
            if self.progress_thread.is_some() {
                return;
            }

            self.progress_stop.store(false, Ordering::SeqCst);
            let stop = Arc::clone(&self.progress_stop);
            let log_path = self.write_log_path.clone();
            self.progress_thread = Some(thread::spawn(move || {
                progress_keepalive_loop(stop, log_path);
            }));
        } else {
            // Stop the keepalive (if any) before emitting the clear
            // sequence so a late re-emission can't reactivate the
            // indicator after we just told the terminal to clear it.
            self.stop_progress_thread();
            self.write(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
        }
    }

    fn flush(&mut self) {
        let _ = io::stdout().flush();
    }

    fn take_input_stream(&mut self) -> Option<InputStream> {
        self.input_stream.take()
    }

    fn kitty_protocol_active(&self) -> bool {
        KEYBOARD_ENHANCEMENT_ACTIVE.load(Ordering::SeqCst)
    }

    fn cell_pixel_size(&self) -> Option<(u32, u32)> {
        let ws = crossterm::terminal::window_size().ok()?;
        if ws.width == 0 || ws.height == 0 || ws.columns == 0 || ws.rows == 0 {
            return None;
        }
        Some((
            u32::from(ws.width) / u32::from(ws.columns),
            u32::from(ws.height) / u32::from(ws.rows),
        ))
    }
}
