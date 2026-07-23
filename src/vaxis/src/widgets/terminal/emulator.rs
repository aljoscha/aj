//! The [`Terminal`] orchestrator: it ties the PTY, the child process, and the
//! emulator core together behind a reader thread and a triple-buffered screen.
//!
//! ## Threads and buffers (per D7)
//!
//! A `Terminal` is a producer/consumer split across three threads:
//!
//! - The reader thread ([`reader_loop`]) reads the child's output off the PTY
//!   master, feeds it to the phase-11a [`Parser`], and mutates the back screen.
//! - The reaper thread ([`reaper_loop`]) watches the child and posts
//!   [`Event::Exited`] when it dies.
//! - The consumer (the application, on its own thread) calls [`Terminal::draw`],
//!   [`Terminal::update`], [`Terminal::resize`], and [`Terminal::try_event`].
//!
//! The two back screens (`back_pri` with scrollback rows, `back_alt` for the
//! alternate screen) plus the mode, tab stops, title, and working directory
//! live in [`Shared`] behind a single `Mutex`. A `dirty` flag in there
//! deduplicates redraws: the reader only enqueues one [`Event::Redraw`] between
//! draws. [`Terminal::draw`] copies the back screen into the main-thread-owned
//! `front` screen (unless synchronized output is active) and blits `front` into
//! the caller's [`Window`]. `front` is never touched by the reader, so it needs
//! no lock.
//!
//! ## Reaping (deviation from upstream, per D7)
//!
//! Upstream installs a process-global `SIGCHLD` handler that reaps any child and
//! routes the exit to the right `Terminal` through a global pid map. That is
//! neither async-signal-safe nor friendly to safe Rust. We instead give each
//! `Terminal` its own reaper thread that owns the [`Child`] behind a `Mutex` and
//! polls [`Child::try_wait`]. Killing on teardown goes through
//! [`Child::kill`], whose internal "already waited" guard closes the
//! PID-recycling race that a blocking `wait` plus a raw `kill(pid)` would open.
//! This costs a short poll interval instead of a blocking wait, which we accept
//! for the race-freedom.
//!
//! ## Teardown
//!
//! On drop we set `should_quit`, kill the child, and join both threads. Killing
//! the child closes its slave fds, so the PTY master reports end-of-input and
//! the reader loop returns on its own. The parent's own slave fd was dropped
//! right after spawn precisely so the master can see that end-of-input.

#![cfg(unix)]

use std::ffi::OsStr;
use std::io::{BufReader, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use thiserror::Error;

use crate::Winsize;
use crate::cell::CursorShape;
use crate::gwidth::Method;
use crate::key::Key;
use crate::widgets::terminal::ansi::{C0, Csi};
use crate::widgets::terminal::command::{Command, CommandError};
use crate::widgets::terminal::key::{EncodeError, encode};
use crate::widgets::terminal::parser::{self, ParseError, Parser};
use crate::widgets::terminal::pty::{self, Pty, PtyError};
use crate::widgets::terminal::screen::Screen;
use crate::window::Window;

/// How often the reaper polls the child for exit.
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// An event the terminal emits to its consumer.
///
/// `TitleChange`/`PwdChange` carry owned strings so the value crosses the
/// channel without borrowing the emulator's scratch buffers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Exited,
    Redraw,
    Bell,
    TitleChange(String),
    PwdChange(String),
}

/// An input event routed from the application into the child.
#[derive(Debug, Clone)]
pub enum InputEvent {
    KeyPress(Key),
}

/// Terminal DEC private modes the emulator honors.
#[derive(Debug, Clone, Copy)]
pub struct Mode {
    pub origin: bool,
    pub autowrap: bool,
    pub cursor: bool,
    pub sync: bool,
}

impl Default for Mode {
    fn default() -> Self {
        Mode {
            origin: false,
            autowrap: true,
            cursor: true,
            sync: false,
        }
    }
}

/// Construction options for a [`Terminal`].
#[derive(Debug, Clone)]
pub struct Options {
    pub scrollback_size: u16,
    pub winsize: Winsize,
    pub initial_working_directory: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            scrollback_size: 500,
            winsize: Winsize {
                rows: 24,
                cols: 80,
                x_pixel: 0,
                y_pixel: 0,
            },
            initial_working_directory: None,
        }
    }
}

/// A failure constructing or driving a [`Terminal`].
#[derive(Debug, Error)]
pub enum TerminalError {
    /// `argv` was empty, so there is no program to run.
    #[error("argv must not be empty")]
    EmptyArgv,

    /// The requested working directory is not absolute.
    #[error("working directory must be an absolute path: {0}")]
    NotAbsolutePath(String),

    #[error(transparent)]
    Pty(#[from] PtyError),

    #[error(transparent)]
    Command(#[from] CommandError),

    /// Encoding a key for the child failed.
    #[error("failed to encode key for the child")]
    KeyEncode(#[source] EncodeError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Which back screen is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveScreen {
    Primary,
    Alternate,
}

/// Emulator state mutated by the reader thread, guarded by one `Mutex`.
struct Shared {
    /// Primary screen: `rows + scrollback` tall.
    back_pri: Screen,
    /// Alternate screen: exactly `rows` tall.
    back_alt: Screen,
    active: ActiveScreen,
    /// True once the back screen changed and a redraw was enqueued, cleared by
    /// [`Terminal::draw`]. Guarded by the same lock as the screens.
    dirty: bool,
    mode: Mode,
    tab_stops: Vec<u16>,
    title: String,
    working_directory: String,
    /// The last grapheme printed, replayed by the REP (`CSI b`) control.
    last_printed: String,
    scrollback_size: u16,
}

impl Shared {
    /// The active back screen.
    fn back(&mut self) -> &mut Screen {
        match self.active {
            ActiveScreen::Primary => &mut self.back_pri,
            ActiveScreen::Alternate => &mut self.back_alt,
        }
    }

    /// Applies one parsed output event to the emulator state.
    fn dispatch(&mut self, event: parser::Event, tx: &Sender<Event>, pty: &PtyMaster) {
        match event {
            parser::Event::Print(bytes) => {
                let Ok(s) = std::str::from_utf8(&bytes) else {
                    return;
                };
                let autowrap = self.mode.autowrap;
                for grapheme in crate::unicode::grapheme_iterator(s) {
                    let gr = grapheme.bytes(s);
                    let w = u8::try_from(crate::gwidth::gwidth(gr, Method::Unicode)).unwrap_or(1);
                    self.back().print(gr, w, autowrap);
                    self.last_printed.clear();
                    self.last_printed.push_str(gr);
                }
            }
            parser::Event::C0(b) => self.handle_c0(b, tx),
            parser::Event::Escape(esc) => self.handle_escape(&esc),
            // Charset shifts are not modeled.
            parser::Event::Ss2(_) | parser::Event::Ss3(_) => {}
            parser::Event::Csi(seq) => self.handle_csi(&seq, pty),
            parser::Event::Osc(osc) => self.handle_osc(&osc, tx),
            // Kitty graphics and other APCs are not modeled here.
            parser::Event::Apc(_) => {}
        }
    }

    fn handle_c0(&mut self, b: C0, tx: &Sender<Event>) {
        match b {
            // EOT is what upstream writes to wake its reader; we treat it as a
            // no-op since our reader wakes on end-of-input instead.
            C0::Nul | C0::Soh | C0::Stx | C0::Eot | C0::Enq | C0::So | C0::Si => {}
            C0::Bel => {
                let _ = tx.send(Event::Bell);
            }
            C0::Bs => self.back().cursor_left(1),
            C0::Ht => self.horizontal_tab(1),
            C0::Lf | C0::Vt | C0::Ff => self.back().index(),
            C0::Cr => self.carriage_return(),
            _ => {}
        }
    }

    fn handle_escape(&mut self, esc: &[u8]) {
        let Some(&final_b) = esc.last() else {
            return;
        };
        match final_b {
            // Charset designation is not modeled.
            b'B' => {}
            b'D' => self.back().index(),
            b'E' => {
                self.back().index();
                self.carriage_return();
            }
            // HTS: set a tab stop at the cursor column.
            b'H' => {
                let col = self.back().cursor.col;
                if !self.tab_stops.contains(&col) {
                    self.tab_stops.push(col);
                    self.tab_stops.sort_unstable();
                }
            }
            b'M' => self.back().reverse_index(),
            _ => {}
        }
    }

    fn handle_csi(&mut self, seq: &Csi, pty: &PtyMaster) {
        match seq.final_byte {
            b'A' | b'k' => {
                let d = first_param(seq, 1);
                self.back().cursor_up(d);
            }
            b'B' => {
                let d = first_param(seq, 1);
                self.back().cursor_down(usize::from(d));
            }
            b'C' => {
                let d = first_param(seq, 1);
                self.back().cursor_right(d);
            }
            b'D' | b'j' => {
                let d = first_param(seq, 1);
                self.back().cursor_left(d);
            }
            b'E' => {
                let d = first_param(seq, 1);
                self.back().cursor_down(usize::from(d));
                self.carriage_return();
            }
            b'F' => {
                let d = first_param(seq, 1);
                self.back().cursor_up(d);
                self.carriage_return();
            }
            // HPA / CHA: absolute column, clamped to the scrolling region.
            b'G' | b'`' => {
                let col = first_param(seq, 1);
                let s = self.back();
                s.cursor.col = col
                    .saturating_sub(1)
                    .clamp(s.scrolling_region.left, s.scrolling_region.right);
                s.cursor.pending_wrap = false;
            }
            // CUP / HVP: absolute row and column.
            b'H' | b'f' => {
                let mut iter = seq.iterator::<u16>();
                let row = iter.next().unwrap_or(1);
                let col = iter.next().unwrap_or(1);
                let s = self.back();
                s.cursor.col = col.saturating_sub(1);
                s.cursor.row = row.saturating_sub(1);
                s.cursor.pending_wrap = false;
            }
            // CHT: advance n tab stops.
            b'I' => {
                let n = first_param(seq, 1);
                self.horizontal_tab(usize::from(n));
            }
            // ED: erase in display.
            b'J' => {
                let kind = first_param(seq, 0);
                let s = self.back();
                match kind {
                    0 => s.erase_below(),
                    1 => s.erase_above(),
                    2 => s.erase_all(),
                    _ => {}
                }
            }
            // EL: erase in line.
            b'K' => {
                let ps = seq.iterator::<u8>().next().unwrap_or(0);
                let s = self.back();
                match ps {
                    0 => s.erase_right(),
                    1 => s.erase_left(),
                    2 => s.erase_line(),
                    _ => {}
                }
            }
            b'L' => {
                let n = first_param(seq, 1);
                self.back().insert_line(usize::from(n));
            }
            b'M' => {
                let n = first_param(seq, 1);
                self.back().delete_line(usize::from(n));
            }
            b'P' => {
                let n = first_param(seq, 1);
                self.back().delete_characters(usize::from(n));
            }
            // SU: scroll up, restoring the cursor afterward.
            b'S' => {
                let n = first_param(seq, 1);
                let s = self.back();
                let cur_row = s.cursor.row;
                let cur_col = s.cursor.col;
                let wrap = s.cursor.pending_wrap;
                s.cursor.col = s.scrolling_region.left;
                s.cursor.row = s.scrolling_region.top;
                s.delete_line(usize::from(n));
                s.cursor.row = cur_row;
                s.cursor.col = cur_col;
                s.cursor.pending_wrap = wrap;
            }
            b'T' => {
                let n = first_param(seq, 1);
                self.back().scroll_down(usize::from(n));
            }
            // DECST8C: reset tab stops to every 8 columns (private, param 5).
            b'W' => {
                if seq.private_marker != Some(b'?') {
                    return;
                }
                if seq.iterator::<u16>().next() != Some(5) {
                    return;
                }
                let width = self.back().width;
                self.tab_stops.clear();
                let mut col = 0u16;
                while col < width {
                    self.tab_stops.push(col);
                    col = col.saturating_add(8);
                }
            }
            // ECH: erase n characters at the cursor.
            b'X' => {
                let n = first_param(seq, 1);
                let s = self.back();
                s.cursor.pending_wrap = false;
                let width = usize::from(s.width);
                let row = usize::from(s.cursor.row);
                let col = usize::from(s.cursor.col);
                let bg = s.cursor.style.bg;
                let start = row * width + col;
                // NOTE: this reproduces upstream's odd `max(row*width+width, n, 1)`
                // end, clamped to the buffer so an out-of-range n cannot panic.
                let end = (row * width + width)
                    .max(usize::from(n))
                    .max(1)
                    .min(s.buf.len());
                for cell in &mut s.buf[start.min(end)..end] {
                    cell.erase(bg);
                }
            }
            b'Z' => {
                let n = first_param(seq, 1);
                self.horizontal_back_tab(usize::from(n));
            }
            // HPR: relative column move.
            b'a' => {
                let n = first_param(seq, 1);
                let origin = self.mode.origin;
                let s = self.back();
                s.cursor.pending_wrap = false;
                let max_end = if origin {
                    s.scrolling_region.right
                } else {
                    s.width.saturating_sub(1)
                };
                s.cursor.col = s
                    .cursor
                    .col
                    .saturating_add(max_end)
                    .min(s.cursor.col.saturating_add(n));
            }
            // REP: repeat the last printed grapheme n times.
            b'b' => {
                let n = first_param(seq, 1);
                let last = std::mem::take(&mut self.last_printed);
                if !last.is_empty() {
                    let w =
                        u8::try_from(crate::gwidth::gwidth(&last, Method::Unicode)).unwrap_or(1);
                    let autowrap = self.mode.autowrap;
                    for _ in 0..n {
                        self.back().print(&last, w, autowrap);
                    }
                }
                self.last_printed = last;
            }
            // DA: device attributes.
            b'c' => match seq.private_marker {
                Some(b'>') => {
                    let _ = pty.write_all(b"\x1b[>1;69;0c");
                }
                Some(b'=') => {
                    let _ = pty.write_all(b"\x1b[=0000c");
                }
                Some(_) => {}
                None => {
                    let _ = pty.write_all(b"\x1b[?62;22c");
                }
            },
            // VPA: absolute row.
            b'd' => {
                let n = first_param(seq, 1);
                let origin = self.mode.origin;
                let s = self.back();
                s.cursor.pending_wrap = false;
                let max = if origin {
                    s.scrolling_region.bottom
                } else {
                    s.height.saturating_sub(1)
                };
                s.cursor.row = max.min(n.saturating_sub(1));
            }
            // VPR: relative row. NOTE: upstream clamps against width, not
            // height. Reproduced faithfully.
            b'e' => {
                let n = first_param(seq, 1);
                let s = self.back();
                s.cursor.pending_wrap = false;
                s.cursor.row = s.width.saturating_sub(1).min(n.saturating_sub(1));
            }
            // TBC: clear tab stops.
            b'g' => {
                let n = first_param(seq, 0);
                let col = self.back().cursor.col;
                match n {
                    0 => self.tab_stops.retain(|&ts| ts != col),
                    3 => self.tab_stops.clear(),
                    _ => {}
                }
            }
            // SM / RM: set/reset mode.
            b'h' | b'l' => {
                let Some(mode) = seq.iterator::<u16>().next() else {
                    return;
                };
                // The only collision is mode 4; we do not support the private
                // form, so skip it.
                if seq.private_marker.is_some() && mode == 4 {
                    return;
                }
                self.set_mode(mode, seq.final_byte == b'h');
            }
            b'm' => {
                if seq.intermediate.is_none() && seq.private_marker.is_none() {
                    self.back().sgr(seq);
                }
            }
            // DSR: device status report.
            b'n' => {
                if seq.intermediate.is_some() || seq.private_marker.is_some() {
                    return;
                }
                let ps = seq.iterator::<u16>().next().unwrap_or(0);
                match ps {
                    5 => {
                        let _ = pty.write_all(b"\x1b[0n");
                    }
                    6 => {
                        let s = self.back();
                        let report = format!("\x1b[{};{}R", s.cursor.row + 1, s.cursor.col + 1);
                        let _ = pty.write_all(report.as_bytes());
                    }
                    _ => {}
                }
            }
            // DECRQM: report mode.
            b'p' => {
                if seq.intermediate != Some(b'$') {
                    return;
                }
                let ps = seq.iterator::<u16>().next().unwrap_or(0);
                match ps {
                    2026 => {
                        let _ = pty.write_all(b"\x1b[?2026;2$p");
                    }
                    _ => {
                        let report = format!("\x1b[?{ps};0$p");
                        let _ = pty.write_all(report.as_bytes());
                    }
                }
            }
            // DECSCUSR (cursor shape) with an intermediate space, or XTVERSION
            // with a `>` private marker.
            b'q' => {
                if seq.intermediate == Some(b' ') {
                    let shape = seq.iterator::<u8>().next().unwrap_or(0);
                    self.back().cursor.shape = cursor_shape_from_u8(shape);
                }
                if seq.private_marker == Some(b'>') {
                    let _ = pty.write_all(b"\x1bP>|vaxis dev\x1b\\");
                }
            }
            // DECSTBM: set the top/bottom scrolling margins.
            b'r' => {
                if seq.intermediate.is_some() || seq.private_marker.is_some() {
                    // XTRESTORE / DECCARA are not modeled.
                    return;
                }
                let mut iter = seq.iterator::<u16>();
                let top = iter.next().unwrap_or(1);
                let origin = self.mode.origin;
                let s = self.back();
                let bottom = iter.next().unwrap_or(s.height);
                s.scrolling_region.top = top.saturating_sub(1);
                s.scrolling_region.bottom = bottom.saturating_sub(1);
                s.cursor.pending_wrap = false;
                if origin {
                    s.cursor.col = s.scrolling_region.left;
                    s.cursor.row = s.scrolling_region.top;
                } else {
                    s.cursor.col = 0;
                    s.cursor.row = 0;
                }
            }
            _ => {}
        }
    }

    fn handle_osc(&mut self, osc: &[u8], tx: &Sender<Event>) {
        let Some(semi) = osc.iter().position(|&b| b == b';') else {
            return;
        };
        let Ok(ps) = std::str::from_utf8(&osc[..semi])
            .unwrap_or("")
            .parse::<u8>()
        else {
            return;
        };
        match ps {
            // OSC 0: set the window title.
            0 => {
                self.title.clear();
                self.title
                    .push_str(&String::from_utf8_lossy(&osc[semi + 1..]));
                let _ = tx.send(Event::TitleChange(self.title.clone()));
            }
            // OSC 7: report the working directory as file://<host><pwd>.
            7 => {
                // Skip the scheme and host to the leading '/' of the path,
                // using upstream's fixed offset past "file://".
                let scheme = b"file://";
                let search_from = semi + 2 + scheme.len() + 1;
                let Some(rel) = osc
                    .get(search_from..)
                    .and_then(|tail| tail.iter().position(|&b| b == b'/'))
                else {
                    return;
                };
                let enc = &osc[search_from + rel..];
                let mut decoded = Vec::with_capacity(enc.len());
                let mut i = 0;
                while i < enc.len() {
                    if enc[i] == b'%' && i + 3 <= enc.len() {
                        if let Ok(byte) = u8::from_str_radix(
                            std::str::from_utf8(&enc[i + 1..i + 3]).unwrap_or(""),
                            16,
                        ) {
                            decoded.push(byte);
                            i += 3;
                            continue;
                        }
                    }
                    decoded.push(enc[i]);
                    i += 1;
                }
                self.working_directory.clear();
                self.working_directory
                    .push_str(&String::from_utf8_lossy(&decoded));
                let _ = tx.send(Event::PwdChange(self.working_directory.clone()));
            }
            _ => {}
        }
    }

    fn set_mode(&mut self, mode: u16, val: bool) {
        match mode {
            7 => self.mode.autowrap = val,
            25 => self.mode.cursor = val,
            // 1049: swap to/from the alternate screen and mark it fully dirty so
            // the next copy repaints the whole window.
            1049 => {
                self.active = if val {
                    ActiveScreen::Alternate
                } else {
                    ActiveScreen::Primary
                };
                for cell in &mut self.back().buf {
                    cell.dirty = true;
                }
            }
            2026 => self.mode.sync = val,
            _ => {}
        }
    }

    fn carriage_return(&mut self) {
        let origin = self.mode.origin;
        let s = self.back();
        s.cursor.pending_wrap = false;
        s.cursor.col = if origin || s.cursor.col >= s.scrolling_region.left {
            s.scrolling_region.left
        } else {
            0
        };
    }

    fn horizontal_tab(&mut self, n: usize) {
        let col = self.back().cursor.col;
        let width = self.back().width;
        let mut i = 0usize;
        let mut final_col = width.saturating_sub(1);
        for &ts in &self.tab_stops {
            if ts <= col {
                continue;
            }
            i += 1;
            if i == n {
                final_col = ts;
                break;
            }
        }
        self.back().cursor_right(final_col.saturating_sub(col));
    }

    fn horizontal_back_tab(&mut self, n: usize) {
        if self.tab_stops.is_empty() {
            return;
        }
        let col = self.back().cursor.col;
        let idx = self
            .tab_stops
            .iter()
            .position(|&ts| ts > col)
            .unwrap_or_else(|| self.tab_stops.len() - 1);
        let stop = self.tab_stops[idx.saturating_sub(n.saturating_sub(1))];
        let origin = self.mode.origin;
        let s = self.back();
        let final_col = if origin {
            stop.max(s.scrolling_region.left)
        } else {
            stop
        };
        // NOTE: upstream computes `final - col` unsigned, which underflows when
        // moving left. We move left by the correct `col - final` instead.
        s.cursor_left(col.saturating_sub(final_col));
    }
}

/// First numeric parameter of `seq`, or `default` when absent.
fn first_param(seq: &Csi, default: u16) -> u16 {
    seq.iterator::<u16>().next().unwrap_or(default)
}

/// Maps a DECSCUSR shape code to a [`CursorShape`].
fn cursor_shape_from_u8(n: u8) -> CursorShape {
    match n {
        1 => CursorShape::BlockBlink,
        2 => CursorShape::Block,
        3 => CursorShape::UnderlineBlink,
        4 => CursorShape::Underline,
        5 => CursorShape::BeamBlink,
        6 => CursorShape::Beam,
        _ => CursorShape::Default,
    }
}

/// Owns the PTY master fd and serializes writes onto it.
///
/// Reads (the reader thread) and writes (the reader thread's replies and the
/// application's key encodings) go to the same fd. Reads and writes on a PTY are
/// independent, so only writes need the lock, and it just keeps two writers from
/// interleaving a control reply with a key sequence.
struct PtyMaster {
    fd: OwnedFd,
    write_lock: Mutex<()>,
}

impl PtyMaster {
    fn new(fd: OwnedFd) -> PtyMaster {
        PtyMaster {
            fd,
            write_lock: Mutex::new(()),
        }
    }

    /// Reads from the master, mapping Linux's post-close `EIO` to end-of-input.
    ///
    /// Once every slave fd is closed the Linux master read returns `EIO` rather
    /// than a zero-length read. We translate it to `Ok(0)` so the parser flushes
    /// the final print run and the reader loop terminates cleanly.
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        match nix::unistd::read(&self.fd, buf) {
            Ok(n) => Ok(n),
            Err(nix::errno::Errno::EIO) => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    fn write_all(&self, mut data: &[u8]) -> std::io::Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while !data.is_empty() {
            let n = nix::unistd::write(&self.fd, data)?;
            if n == 0 {
                break;
            }
            data = &data[n..];
        }
        Ok(())
    }

    fn set_size(&self, ws: Winsize) -> Result<(), PtyError> {
        pty::set_winsize(self.fd.as_raw_fd(), ws)
    }
}

/// A [`Read`] adapter over the shared PTY master for the reader's `BufReader`.
struct MasterReader {
    master: Arc<PtyMaster>,
}

impl Read for MasterReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.master.read(buf)
    }
}

/// A virtual terminal widget backed by a child process on a PTY.
pub struct Terminal {
    master: Arc<PtyMaster>,
    /// The parent's slave fd, held until [`Terminal::spawn`] hands stdio to the
    /// child and then dropped so the master can observe the child's exit.
    slave: Option<OwnedFd>,
    command: Command,
    working_directory_init: Option<PathBuf>,
    /// The screen we blit from. Owned by the consumer thread, never the reader.
    front: Screen,
    /// Cached `mode.cursor` from the last successful lock, so a draw that races
    /// the reader still knows whether to show the cursor.
    cursor_on: bool,
    shared: Arc<Mutex<Shared>>,
    should_quit: Arc<AtomicBool>,
    events_tx: Sender<Event>,
    events_rx: Receiver<Event>,
    child: Option<Arc<Mutex<Child>>>,
    reader_handle: Option<JoinHandle<()>>,
    reaper_handle: Option<JoinHandle<()>>,
}

impl Terminal {
    /// Builds a terminal: validates the working directory, opens and sizes the
    /// PTY, builds tab stops every 8 columns, and allocates the three screens.
    ///
    /// The child is not started until [`Terminal::spawn`].
    pub fn new<S: AsRef<OsStr>>(argv: &[S], opts: Options) -> Result<Terminal, TerminalError> {
        let Some((program, args)) = argv.split_first() else {
            return Err(TerminalError::EmptyArgv);
        };
        if let Some(pwd) = &opts.initial_working_directory {
            if !pwd.is_absolute() {
                return Err(TerminalError::NotAbsolutePath(pwd.display().to_string()));
            }
        }

        let pty = Pty::open()?;
        pty.set_size(opts.winsize)?;
        let Pty { master, slave } = pty;

        let mut command = Command::new(
            program.as_ref().to_os_string(),
            args.iter().map(|a| a.as_ref().to_os_string()).collect(),
        );
        if let Some(dir) = &opts.initial_working_directory {
            command.set_working_directory(dir.clone());
        }

        let cols = opts.winsize.cols;
        let rows = opts.winsize.rows;
        let back_rows = rows.saturating_add(opts.scrollback_size);

        let mut tab_stops = Vec::new();
        let mut col = 0u16;
        while col < cols {
            tab_stops.push(col);
            col = col.saturating_add(8);
        }

        let shared = Shared {
            back_pri: Screen::new(cols, back_rows),
            back_alt: Screen::new(cols, rows),
            active: ActiveScreen::Primary,
            dirty: false,
            mode: Mode::default(),
            tab_stops,
            title: String::new(),
            working_directory: String::new(),
            last_printed: String::new(),
            scrollback_size: opts.scrollback_size,
        };

        let (events_tx, events_rx) = channel();

        Ok(Terminal {
            master: Arc::new(PtyMaster::new(master)),
            slave: Some(slave),
            command,
            working_directory_init: opts.initial_working_directory,
            front: Screen::new(cols, rows),
            cursor_on: true,
            shared: Arc::new(Mutex::new(shared)),
            should_quit: Arc::new(AtomicBool::new(false)),
            events_tx,
            events_rx,
            child: None,
            reader_handle: None,
            reaper_handle: None,
        })
    }

    /// Spawns the child on the PTY slave and launches the reader and reaper
    /// threads. A second call is a no-op.
    pub fn spawn(&mut self) -> Result<(), TerminalError> {
        if self.reader_handle.is_some() {
            return Ok(());
        }
        let Some(slave) = self.slave.take() else {
            return Ok(());
        };

        lock(&self.shared).active = ActiveScreen::Primary;

        let child = self.command.spawn(&slave)?;
        // The parent's slave copy drops at the end of this scope, so once the
        // child exits and closes its own stdio the master reports end-of-input.

        let cwd = match &self.working_directory_init {
            Some(p) => p.to_string_lossy().into_owned(),
            None => std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        {
            let mut sh = lock(&self.shared);
            sh.working_directory.clear();
            sh.working_directory.push_str(&cwd);
        }

        let child = Arc::new(Mutex::new(child));
        self.child = Some(Arc::clone(&child));

        let reaper_tx = self.events_tx.clone();
        self.reaper_handle = Some(thread::spawn(move || reaper_loop(child, reaper_tx)));

        let reader_shared = Arc::clone(&self.shared);
        let reader_master = Arc::clone(&self.master);
        let reader_tx = self.events_tx.clone();
        let reader_quit = Arc::clone(&self.should_quit);
        self.reader_handle = Some(thread::spawn(move || {
            reader_loop(reader_shared, reader_master, reader_tx, reader_quit);
        }));

        Ok(())
    }

    /// Copies the back screen into `front` and blits `front` into `win`.
    ///
    /// Takes the back lock without blocking: if the reader holds it (or
    /// synchronized output is active) this frame reuses the previous `front`.
    /// Should only be called from the consumer thread.
    pub fn draw(&mut self, win: &Window) {
        if let Ok(mut sh) = self.shared.try_lock() {
            // Kept as a separate check so we hold the lock but skip the copy
            // mid-synchronized-update, rather than deadlocking on it.
            if !sh.mode.sync {
                sh.back().copy_to(&mut self.front);
                sh.dirty = false;
            }
            self.cursor_on = sh.mode.cursor;
        }

        let mut row = 0;
        while row < self.front.height {
            let mut col = 0;
            while col < self.front.width {
                let Some(cell) = self.front.read_cell(usize::from(col), usize::from(row)) else {
                    col += 1;
                    continue;
                };
                let advance = u16::from(cell.char.width.max(1));
                win.write_cell(col, row, cell);
                col += advance;
            }
            row += 1;
        }

        if self.cursor_on {
            win.set_cursor_shape(self.front.cursor.shape);
            win.show_cursor(self.front.cursor.col, self.front.cursor.row);
        }
    }

    /// Pops the next emulator event, or `None` when the queue is empty.
    pub fn try_event(&self) -> Option<Event> {
        self.events_rx.try_recv().ok()
    }

    /// Routes an input event to the child by encoding it onto the PTY.
    pub fn update(&mut self, event: InputEvent) -> Result<(), TerminalError> {
        match event {
            InputEvent::KeyPress(key) => {
                let flags = lock(&self.shared).back().csi_u_flags;
                let mut buf = Vec::new();
                encode(&mut buf, &key, true, flags).map_err(TerminalError::KeyEncode)?;
                self.master.write_all(&buf)?;
            }
        }
        Ok(())
    }

    /// Resizes the screens and the PTY. A no-op when the size is unchanged, so
    /// it is cheap to call every frame. Should only be called from the consumer
    /// thread.
    pub fn resize(&mut self, ws: Winsize) -> Result<(), TerminalError> {
        if ws.cols == self.front.width && ws.rows == self.front.height {
            return Ok(());
        }
        {
            let mut sh = lock(&self.shared);
            let back_rows = ws.rows.saturating_add(sh.scrollback_size);
            self.front = Screen::new(ws.cols, ws.rows);
            sh.back_pri = Screen::new(ws.cols, back_rows);
            sh.back_alt = Screen::new(ws.cols, ws.rows);
        }
        self.master.set_size(ws)?;
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.should_quit.store(true, Ordering::Relaxed);
        // Kill the child so it closes its slave fds. That drives the master to
        // end-of-input, which unblocks and ends the reader loop. `Child::kill`
        // is a no-op if the reaper already reaped, so there is no risk of
        // signalling a recycled PID.
        if let Some(child) = &self.child {
            let _ = lock(child).kill();
        }
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.reaper_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Locks a mutex, recovering the guard if a previous holder panicked.
///
/// Our critical sections here do not panic in normal operation, so a poisoned
/// lock means an earlier bug already surfaced. We recover rather than cascade a
/// second panic through the reader/reaper/consumer threads.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The reader thread: parse the child's output and drive the back screen.
fn reader_loop(
    shared: Arc<Mutex<Shared>>,
    master: Arc<PtyMaster>,
    tx: Sender<Event>,
    quit: Arc<AtomicBool>,
) {
    let mut parser = Parser::new();
    let mut reader = BufReader::new(MasterReader {
        master: Arc::clone(&master),
    });

    loop {
        if quit.load(Ordering::Relaxed) {
            break;
        }
        match parser.parse_reader(&mut reader) {
            Ok(event) => {
                let mut sh = lock(&shared);
                // Deduplicate redraws: enqueue at most one between draws, then
                // set `dirty` until a draw copies the screen out.
                if !sh.dirty {
                    let _ = tx.send(Event::Redraw);
                    sh.dirty = true;
                }
                sh.dispatch(event, &tx, &master);
            }
            // End-of-input (child gone) or a malformed stream ends the loop.
            Err(ParseError::Eof) => break,
            Err(_) => break,
        }
    }
}

/// The reaper thread: post [`Event::Exited`] once the child dies.
fn reaper_loop(child: Arc<Mutex<Child>>, tx: Sender<Event>) {
    loop {
        {
            let mut guard = lock(&child);
            match guard.try_wait() {
                Ok(Some(_status)) => {
                    let _ = tx.send(Event::Exited);
                    return;
                }
                Ok(None) => {}
                Err(_) => {
                    let _ = tx.send(Event::Exited);
                    return;
                }
            }
        }
        thread::sleep(REAP_POLL_INTERVAL);
    }
}
