//! Headless virtual terminal backed by a VT100 parser, for integration tests.
//!
//! The [`VirtualTerminal`] implements [`aj_tui::terminal::Terminal`] so a `Tui`
//! can write into it exactly as it would into a real terminal. Tests keep a
//! [`Clone`]d handle and read back what rendered — viewport lines, cursor
//! position, or per-cell attributes — to assert on rendering behavior.
//!
//! Internally the VT parser sits behind an `Rc<RefCell<_>>`, so the handle
//! passed to the `Tui` and the handle held by the test share a single
//! underlying parser without needing separate ownership plumbing.
//!
//! Synthetic input flows through a `tokio::mpsc` channel: [`Tui::start`]
//! takes the receiver side via [`aj_tui::terminal::Terminal::take_input_stream`],
//! and tests push events through the sender returned by
//! [`VirtualTerminal::input_sender`].

use std::cell::{Ref, RefCell};
use std::io;
use std::rc::Rc;

use aj_tui::keys::InputEvent;
use aj_tui::terminal::{InputStream, Terminal};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use vt100_ctt::{Color, Parser};

const DEFAULT_SCROLLBACK: usize = 100;

/// Inner state shared between cloned `VirtualTerminal` handles.
struct State {
    parser: Parser,
    columns: u16,
    rows: u16,
    /// Sender side of the input channel. Cloned out via
    /// [`VirtualTerminal::input_sender`] so tests can push synthetic
    /// events.
    input_tx: mpsc::UnboundedSender<InputEvent>,
    /// Receiver side of the input channel. Moved out exactly once, by
    /// the first [`Terminal::take_input_stream`] call.
    input_rx: Option<mpsc::UnboundedReceiver<InputEvent>>,
    cursor_visible: bool,
    title: String,
    progress_active: bool,
    kitty_protocol_active: bool,
    /// Every `write` call, captured verbatim. Used directly by
    /// [`super::logging_terminal::LoggingVirtualTerminal`] and available to
    /// tests that need to assert on emitted escape sequences.
    writes: Vec<String>,
    /// Number of times [`Terminal::start`] has been invoked on this handle.
    /// The override is a no-op side-effect-wise (nothing is written), so the
    /// count is purely an observability hook for tests that want to assert
    /// on startup/shutdown lifecycle invariants.
    start_count: usize,
    /// Number of times [`Terminal::stop`] has been invoked on this handle.
    stop_count: usize,
}

impl State {
    fn new(columns: u16, rows: u16) -> Self {
        let parser = Parser::new(rows, columns, DEFAULT_SCROLLBACK);
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        Self {
            parser,
            columns,
            rows,
            input_tx,
            input_rx: Some(input_rx),
            cursor_visible: true,
            title: String::new(),
            progress_active: false,
            // Default to true so tests that don't care about protocol
            // negotiation see components as if Kitty key reporting were
            // enabled. Tests that care can override via
            // [`VirtualTerminal::set_kitty_protocol_active`].
            kitty_protocol_active: true,
            writes: Vec::new(),
            start_count: 0,
            stop_count: 0,
        }
    }
}

/// A headless terminal implementation for tests.
///
/// Cloning this handle yields another reference to the same underlying
/// terminal — do that to hand one copy to `Tui::new` and keep another for
/// assertions:
///
/// ```ignore
/// let terminal = VirtualTerminal::new(40, 10);
/// let mut tui = Tui::new(Box::new(terminal.clone()));
/// tui.render();
/// assert_eq!(terminal.viewport()[0], "hello");
/// ```
pub struct VirtualTerminal {
    state: Rc<RefCell<State>>,
}

impl VirtualTerminal {
    /// Create a new virtual terminal with the given dimensions.
    pub fn new(columns: u16, rows: u16) -> Self {
        Self {
            state: Rc::new(RefCell::new(State::new(columns, rows))),
        }
    }

    // -- Viewport and grid inspection --

    /// Return the visible viewport as one trimmed string per row.
    ///
    /// Rows are trimmed of trailing whitespace, matching what a user
    /// would see on screen if every cell beyond the last visible
    /// glyph were empty. Assertions like
    /// `terminal.viewport()[0] == "hello"` work whether or not the
    /// component pads with explicit trailing spaces.
    ///
    /// The returned vector always has exactly `rows` entries, padding
    /// with empty strings if the parser returned fewer lines than the
    /// viewport height.
    pub fn viewport(&self) -> Vec<String> {
        let state = self.state.borrow();
        let rows = usize::from(state.rows);
        let contents = state.parser.screen().contents();
        let mut lines: Vec<String> = contents
            .split('\n')
            .map(|line| line.trim_end_matches(' ').to_string())
            .collect();
        lines.resize(rows, String::new());
        lines
    }

    /// Return the viewport concatenated into a single string with `\n`
    /// separators. Handy shortcut for snapshot-style comparisons.
    pub fn viewport_text(&self) -> String {
        self.viewport().join("\n")
    }

    /// Return the viewport with trailing empty rows removed.
    ///
    /// Convenience for diff-friendly snapshots: a 24-row `VirtualTerminal`
    /// that only rendered two lines of content returns `["hello", "world"]`
    /// instead of `["hello", "world", "", "", ..., ""]`. Interior blanks
    /// are preserved.
    pub fn viewport_trimmed(&self) -> Vec<String> {
        let mut lines = self.viewport();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines
    }

    /// Return the entire buffer — every scrollback row followed by the
    /// current viewport — as one trimmed string per row.
    ///
    /// Rows are in top-to-bottom order: oldest scrollback first, then the
    /// current viewport. The returned vector has `scrollback_len + rows`
    /// entries. Rows that have no content yet are returned as empty
    /// strings.
    ///
    /// This is the backing reader for assertions that need to see what
    /// scrolled off-screen — e.g. verifying that streamed lines weren't
    /// overwritten when tool output pushed them into scrollback.
    ///
    /// Current scrollback offset is preserved across the call.
    pub fn scroll_buffer(&self) -> Vec<String> {
        let mut state = self.state.borrow_mut();
        let rows = usize::from(state.rows);
        let prev_offset = state.parser.screen().scrollback();

        // Set offset past the end so the next read of `scrollback()` gives
        // us the real length. `set_scrollback` clamps to the actual size.
        state.parser.screen_mut().set_scrollback(usize::MAX);
        let scrollback_len = state.parser.screen().scrollback();

        let mut lines = Vec::with_capacity(scrollback_len + rows);

        // Walk the scrollback top-down by stepping the offset from max to
        // 1, grabbing only the topmost visible row each time. At offset=N,
        // the topmost visible row is `scrollback[scrollback_len - N]`, so
        // offsets `scrollback_len, scrollback_len-1, ..., 1` yield oldest
        // to most-recent scrollback rows with no overlap.
        for offset in (1..=scrollback_len).rev() {
            state.parser.screen_mut().set_scrollback(offset);
            let contents = state.parser.screen().contents();
            let first = contents.split('\n').next().unwrap_or("").to_string();
            lines.push(first);
        }

        // Final read at offset=0 is the current viewport.
        state.parser.screen_mut().set_scrollback(0);
        let viewport = state.parser.screen().contents();
        let mut viewport_lines: Vec<String> = viewport.split('\n').map(str::to_string).collect();
        viewport_lines.resize(rows, String::new());
        lines.extend(viewport_lines);

        // Restore the caller's scrollback offset.
        state.parser.screen_mut().set_scrollback(prev_offset);
        lines
    }

    /// Get the contents of the cell at `(row, col)`, or `None` if out of
    /// bounds. Returns an owned [`CellInfo`] so tests don't have to hold onto
    /// a borrow of the underlying parser.
    pub fn cell(&self, row: u16, col: u16) -> Option<CellInfo> {
        let state = self.state.borrow();
        state.parser.screen().cell(row, col).map(CellInfo::from)
    }

    /// Snapshot every cell in `row` as a left-to-right vector of
    /// [`CellInfo`], or `None` if `row` is out of bounds.
    ///
    /// Useful when an assertion cares about styling across a whole line
    /// (e.g. "every cell in the heading row is bold and cyan") without
    /// having to loop over [`Self::cell`] manually. Wide-character
    /// continuation cells are included in their own right — callers that
    /// want only the lead cells can filter on
    /// [`CellInfo::is_wide_continuation`].
    pub fn cells_in_row(&self, row: u16) -> Option<Vec<CellInfo>> {
        let state = self.state.borrow();
        let screen = state.parser.screen();
        // Use the parser's authoritative size rather than the cached
        // `state.rows` / `state.columns`. The cache is updated alongside
        // the parser by `resize` (see [`Self::resize`]) so they should
        // always match — but if a future change ever introduces drift,
        // sourcing the bounds from the parser keeps `cells_in_row` from
        // silently emitting trailing `CellInfo::default()` entries
        // beyond the parser's actual width.
        let (parser_rows, parser_cols) = screen.size();
        if row >= parser_rows {
            return None;
        }
        let cells = (0..parser_cols)
            .map(|col| {
                screen
                    .cell(row, col)
                    .map(CellInfo::from)
                    .unwrap_or_default()
            })
            .collect();
        Some(cells)
    }

    /// Current cursor position as `(row, col)`.
    pub fn cursor(&self) -> (u16, u16) {
        self.state.borrow().parser.screen().cursor_position()
    }

    /// Whether the cursor was last hidden via [`Terminal::hide_cursor`].
    pub fn is_cursor_visible(&self) -> bool {
        self.state.borrow().cursor_visible
    }

    /// Current terminal title, as last set via [`Terminal::set_title`].
    pub fn title(&self) -> String {
        self.state.borrow().title.clone()
    }

    /// Whether the OSC 9;4 progress indicator is currently active, as
    /// last set via [`Terminal::set_progress`]. Defaults to `false`.
    pub fn is_progress_active(&self) -> bool {
        self.state.borrow().progress_active
    }

    /// Number of times [`Terminal::start`] has been called on this handle
    /// (or any clone of it, since state is shared). The override is a no-op,
    /// so this exists purely so tests can assert on lifecycle invariants
    /// (e.g. "start was called exactly once before stop").
    pub fn start_count(&self) -> usize {
        self.state.borrow().start_count
    }

    /// Number of times [`Terminal::stop`] has been called on this handle
    /// (or any clone of it).
    pub fn stop_count(&self) -> usize {
        self.state.borrow().stop_count
    }

    // -- Raw write access --

    /// Return a borrow of every `write()` call made against this terminal,
    /// in order.
    pub fn writes(&self) -> Ref<'_, Vec<String>> {
        Ref::map(self.state.borrow(), |s| &s.writes)
    }

    /// Concatenate every captured write into a single string.
    pub fn writes_joined(&self) -> String {
        self.state.borrow().writes.join("")
    }

    /// Discard the captured write log. The VT parser state is unaffected.
    pub fn clear_writes(&self) {
        self.state.borrow_mut().writes.clear();
    }

    // -- Out-of-band terminal state manipulation --

    /// Clear the viewport and move the cursor home, the way a test helper
    /// would do it outside the component stream. Unlike calling
    /// [`Terminal::clear_screen`] through the `Tui`, this does *not* append
    /// to the captured `writes` log — it's equivalent to the user hitting a
    /// "clear" button in their terminal.
    pub fn clear_viewport(&self) {
        let mut state = self.state.borrow_mut();
        state.parser.process(b"\x1b[2J\x1b[H");
    }

    /// Reset the virtual terminal to a pristine state: a fresh VT parser at
    /// the current dimensions, cursor visible, no title, and no captured
    /// writes or queued input. Useful between phases of a multi-stage test.
    pub fn reset(&self) {
        let mut state = self.state.borrow_mut();
        state.parser = Parser::new(state.rows, state.columns, DEFAULT_SCROLLBACK);
        state.cursor_visible = true;
        state.title.clear();
        state.progress_active = false;
        state.writes.clear();
        // Drain any pending input events without dropping the channel:
        // the sender end may still be in use by the test or other
        // components.
        if let Some(rx) = state.input_rx.as_mut() {
            while rx.try_recv().is_ok() {}
        }
        state.start_count = 0;
        state.stop_count = 0;
    }

    // -- Input simulation --
    //
    // Integration tests drive components directly through `Tui::handle_input`
    // (or the component's own `handle_input`), so there is no need for
    // byte-stream or typed-event simulators on the terminal itself. The one
    // exception is `resize`, which must also update the terminal dimensions
    // and therefore lives naturally as a method here; it pushes a
    // `Resize` event into the input channel so async tests driving
    // `Tui::next_event` see the notification.

    /// Resize the terminal to `(columns, rows)` and queue a `Resize` input
    /// event so the `Tui`'s event loop can react.
    pub fn resize(&self, columns: u16, rows: u16) {
        let mut state = self.state.borrow_mut();
        state.columns = columns;
        state.rows = rows;
        state.parser.screen_mut().set_size(rows, columns);
        let _ = state.input_tx.send(InputEvent::Resize(columns, rows));
    }

    /// Get a cloneable sender for pushing synthetic [`InputEvent`]s
    /// into the terminal's input stream. The stream itself is taken by
    /// [`Tui::start`] via [`Terminal::take_input_stream`]; tests keep
    /// a sender clone to inject events.
    pub fn input_sender(&self) -> mpsc::UnboundedSender<InputEvent> {
        self.state.borrow().input_tx.clone()
    }

    /// Override whether Kitty keyboard protocol reporting is considered
    /// active. Defaults to `true`. Setting this on a clone affects every
    /// other handle because state is shared behind an `Rc<RefCell<_>>`.
    pub fn set_kitty_protocol_active(&self, active: bool) {
        self.state.borrow_mut().kitty_protocol_active = active;
    }
}

impl Clone for VirtualTerminal {
    fn clone(&self) -> Self {
        Self {
            state: Rc::clone(&self.state),
        }
    }
}

impl Terminal for VirtualTerminal {
    fn write(&mut self, data: &str) {
        let mut state = self.state.borrow_mut();
        state.writes.push(data.to_string());
        state.parser.process(data.as_bytes());
    }

    fn start(&mut self) -> io::Result<()> {
        // No real terminal state to bring up; just record that start was
        // called so tests can assert on it. The trait contract still
        // requires this be idempotent, and incrementing is — tests that
        // care read the count, they don't check for a single-shot flag.
        self.state.borrow_mut().start_count += 1;
        Ok(())
    }

    fn stop(&mut self) {
        self.state.borrow_mut().stop_count += 1;
    }

    fn columns(&self) -> u16 {
        self.state.borrow().columns
    }

    fn rows(&self) -> u16 {
        self.state.borrow().rows
    }

    fn move_by(&mut self, lines: i32) {
        if lines < 0 {
            let esc = format!("\x1b[{}A", -lines);
            self.write(&esc);
        } else if lines > 0 {
            let esc = format!("\x1b[{}B", lines);
            self.write(&esc);
        }
    }

    fn hide_cursor(&mut self) {
        self.state.borrow_mut().cursor_visible = false;
        self.write("\x1b[?25l");
    }

    fn show_cursor(&mut self) {
        self.state.borrow_mut().cursor_visible = true;
        self.write("\x1b[?25h");
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
        self.state.borrow_mut().title = title.to_string();
        let esc = format!("\x1b]0;{}\x07", title);
        self.write(&esc);
    }

    fn set_progress(&mut self, active: bool) {
        self.state.borrow_mut().progress_active = active;
        if active {
            self.write("\x1b]9;4;1;0\x07");
        } else {
            self.write("\x1b]9;4;0;\x07");
        }
    }

    fn flush(&mut self) {
        // No-op: writes are applied to the parser synchronously.
    }

    fn take_input_stream(&mut self) -> Option<InputStream> {
        let rx = self.state.borrow_mut().input_rx.take()?;
        Some(Box::pin(UnboundedReceiverStream::new(rx)))
    }

    fn kitty_protocol_active(&self) -> bool {
        self.state.borrow().kitty_protocol_active
    }
}

// ---------------------------------------------------------------------------
// CellInfo: an owned snapshot of a parser cell
// ---------------------------------------------------------------------------

/// An owned snapshot of a single terminal cell's visible contents and style.
#[derive(Debug, Clone)]
pub struct CellInfo {
    pub contents: String,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub is_wide: bool,
    pub is_wide_continuation: bool,
}

impl Default for CellInfo {
    fn default() -> Self {
        Self {
            contents: String::new(),
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
            is_wide: false,
            is_wide_continuation: false,
        }
    }
}

impl From<&vt100_ctt::Cell> for CellInfo {
    fn from(c: &vt100_ctt::Cell) -> Self {
        Self {
            contents: c.contents().to_string(),
            fg: c.fgcolor(),
            bg: c.bgcolor(),
            bold: c.bold(),
            italic: c.italic(),
            underline: c.underline(),
            inverse: c.inverse(),
            is_wide: c.is_wide(),
            is_wide_continuation: c.is_wide_continuation(),
        }
    }
}
