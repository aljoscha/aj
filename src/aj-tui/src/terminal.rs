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
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
    /// Passing `true` signals "work in progress" (OSC 9;4;1;0, the
    /// "indeterminate" value on terminals that implement the ConEmu
    /// progress protocol — notably Windows Terminal, ConEmu, and some
    /// tmux/iTerm2 builds). Passing `false` clears it (OSC 9;4;0;).
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

/// A real terminal connected to stdin/stdout.
pub struct ProcessTerminal {
    /// Whether `start()` has run and `stop()` has not yet been called.
    started: bool,
    /// `Some` until the first call to [`ProcessTerminal::take_input_stream`];
    /// holds the crossterm-backed input stream waiting to be handed to
    /// the [`crate::tui::Tui`] event loop.
    input_stream: Option<InputStream>,
    /// Destination for the optional [`WRITE_LOG_ENV`] write-log. `None`
    /// means the env var was unset, so writes bypass the disk logger
    /// entirely.
    write_log_path: Option<PathBuf>,
}

impl ProcessTerminal {
    pub fn new() -> Self {
        // Build the crossterm-backed input stream up front so it's
        // available before `start()` is called. The underlying
        // `EventStream` doesn't actually read from stdin until polled,
        // so creating it early is cheap and side-effect-free.
        let events = crossterm::event::EventStream::new()
            .filter_map(|ev| async move { ev.ok().and_then(|e| InputEvent::try_from(e).ok()) });
        let input_stream: InputStream = Box::pin(events);
        Self {
            started: false,
            input_stream: Some(input_stream),
            write_log_path: resolve_write_log_path(),
        }
    }

    /// Path the write-log is currently being appended to, if any. Read
    /// by tests; never surface this to end-user-facing API.
    pub fn write_log_path(&self) -> Option<&Path> {
        self.write_log_path.as_deref()
    }
}

impl Default for ProcessTerminal {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ProcessTerminal {
    fn drop(&mut self) {
        // Safety net: restore terminal state even if the caller forgot to
        // call `stop()` (or `Tui::stop()`), or if a panic unwound past
        // `stop()` without going through the panic hook.
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
        restore_terminal_state();
        self.started = false;
    }

    fn columns(&self) -> u16 {
        terminal::size().map(|(w, _)| w).unwrap_or(80)
    }

    fn rows(&self) -> u16 {
        terminal::size().map(|(_, h)| h).unwrap_or(24)
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
        // OSC 9;4 progress protocol (ConEmu / Windows Terminal):
        //   1;0  — indeterminate / pulsing
        //   0;   — clear. The trailing `;` is harmless on terminals
        //          that implement the protocol and is ignored on ones
        //          that don't.
        if active {
            self.write("\x1b]9;4;1;0\x07");
        } else {
            self.write("\x1b]9;4;0;\x07");
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
}
