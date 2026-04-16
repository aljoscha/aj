//! The main TUI rendering engine.
//!
//! Manages the render loop with differential updates, overlay compositing,
//! focus management, and synchronized output.
//!
//! # Async event loop
//!
//! A `Tui` is driven by its own `tokio::select!` loop. Callers build the
//! `Tui`, call [`Tui::start`] to bring up the terminal, and then drive
//! [`Tui::next_event`] in a loop:
//!
//! ```ignore
//! let mut tui = Tui::new(Box::new(ProcessTerminal::new()));
//! tui.start()?;
//! // Components are added via `tui.add_child(...)`; overlays ride along.
//! loop {
//!     match tui.next_event().await {
//!         Some(TuiEvent::Input(ev)) if ev.is_ctrl('c') => break,
//!         Some(TuiEvent::Input(ev)) => tui.handle_input(&ev),
//!         Some(TuiEvent::Render) => tui.render(),
//!         None => break,
//!     }
//! }
//! tui.stop();
//! ```
//!
//! Async tasks that want to wake the loop (streaming LLM, spinner tick,
//! file watcher, the editor's autocomplete worker) clone a
//! [`RenderHandle`] out of [`Tui::handle`] and call
//! [`RenderHandle::request_render`]. Multiple requests inside one
//! throttle window collapse into a single [`TuiEvent::Render`].

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant as StdInstant, SystemTime, UNIX_EPOCH};

use futures::stream::StreamExt;
use tokio::sync::mpsc;
use tokio::time::{Instant as TokioInstant, Interval, MissedTickBehavior, interval_at};

use crate::ansi::{
    SEGMENT_RESET, extract_segments, slice_by_column, slice_with_width, visible_width,
};
use crate::component::{CURSOR_MARKER, Component};
use crate::container::Container;
use crate::keys::{InputEvent, key_id_matches};
use crate::terminal::{InputStream, Terminal};

/// Minimum interval between renders (~60fps).
const MIN_RENDER_INTERVAL: Duration = Duration::from_millis(16);

/// Synchronized output: begin (terminal buffers all output).
const SYNC_BEGIN: &str = "\x1b[?2026h";
/// Synchronized output: end (terminal flushes atomically).
const SYNC_END: &str = "\x1b[?2026l";

/// Events yielded by [`Tui::next_event`]. Exactly one kind of thing
/// happens per loop iteration: the application routes it into the
/// `Tui` accordingly.
#[derive(Debug, Clone)]
pub enum TuiEvent {
    /// User input arrived from the terminal's input stream.
    Input(InputEvent),
    /// The render throttle fired with at least one pending request; the
    /// application should now invoke [`Tui::render`].
    Render,
}

/// Cloneable handle for requesting renders from async tasks.
///
/// Requests are non-blocking and coalesced by the [`Tui`]'s render
/// throttle: calling [`RenderHandle::request_render`] a thousand times
/// in a tight loop produces at most one [`TuiEvent::Render`] per
/// `render_interval`.
///
/// Dropping every clone of the handle and the [`Tui`] closes the
/// render channel; [`Tui::next_event`] will then only yield pending
/// renders and input events.
///
/// The handle also carries the most recent terminal dimensions
/// observed by the [`Tui`]. Components that need to size themselves
/// to the terminal (auto-sizing scroll windows, popup heights, etc.)
/// can read [`RenderHandle::terminal_rows`] /
/// [`RenderHandle::terminal_columns`] without having to thread the
/// dimensions through every render call. Both default to `0` until
/// the [`Tui`] has read its terminal at least once (in
/// [`Tui::start`] or the first [`Tui::render`]).
#[derive(Debug, Clone)]
pub struct RenderHandle {
    tx: mpsc::UnboundedSender<()>,
    term_rows: Arc<AtomicU16>,
    term_cols: Arc<AtomicU16>,
}

impl RenderHandle {
    /// Build a no-op handle not connected to any [`Tui`].
    ///
    /// `request_render` calls become silent no-ops (the underlying
    /// receiver is dropped immediately on construction so sends fail
    /// the same way they do on a stopped `Tui`). `terminal_rows` /
    /// `terminal_columns` both read `0`.
    ///
    /// Useful in tests and standalone component construction where the
    /// component needs *some* handle to satisfy its constructor but
    /// has no `Tui` to wire into. Production code should always pass
    /// a real handle from [`Tui::handle`].
    pub fn detached() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel::<()>();
        // _rx drops here; subsequent `tx.send` calls return Err and
        // are silently ignored by `request_render`.
        Self {
            tx,
            term_rows: Arc::new(AtomicU16::new(0)),
            term_cols: Arc::new(AtomicU16::new(0)),
        }
    }

    /// Ask the driver to schedule a render. Safe to call from any task.
    pub fn request_render(&self) {
        // Ignoring the error is intentional: if the Tui has shut down,
        // a dropped signal is the correct observable outcome.
        let _ = self.tx.send(());
    }

    /// Most recent terminal height (rows) observed by the owning
    /// [`Tui`]. Returns `0` if the `Tui` has not yet read its terminal
    /// (no [`Tui::start`] / [`Tui::render`] has run).
    pub fn terminal_rows(&self) -> u16 {
        self.term_rows.load(Ordering::Relaxed)
    }

    /// Most recent terminal width (columns) observed by the owning
    /// [`Tui`]. Returns `0` if the `Tui` has not yet read its terminal
    /// (no [`Tui::start`] / [`Tui::render`] has run).
    pub fn terminal_columns(&self) -> u16 {
        self.term_cols.load(Ordering::Relaxed)
    }
}

/// Environments where a height change is a frequent, nuisance event (e.g.
/// Termux toggling its on-screen keyboard). Full redraws in those envs
/// replay the whole scrollback and are visually disruptive, so we drop to
/// the differential path even on height change.
fn should_skip_full_redraw_on_height_change() -> bool {
    std::env::var("TERMUX_VERSION").is_ok()
}

// ---------------------------------------------------------------------------
// Overlay types
// ---------------------------------------------------------------------------

/// Anchor position for overlays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAnchor {
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

/// A size value that can be absolute or percentage-based.
#[derive(Debug, Clone, Copy)]
pub enum SizeValue {
    /// Absolute number of columns/rows.
    Absolute(usize),
    /// Percentage of the available space.
    Percent(f32),
}

impl SizeValue {
    /// Resolve to an absolute value given a reference dimension.
    pub fn resolve(&self, reference: usize) -> usize {
        match self {
            SizeValue::Absolute(n) => *n,
            // Percent math is the one place we genuinely need lossy
            // numeric casts: there's no stable safe `usize → f32` or
            // `f32 → usize`. Terminal dimensions stay well below the
            // f32 precision threshold (2^24), so the round-trip is
            // exact in practice.
            #[allow(clippy::as_conversions)]
            SizeValue::Percent(p) => ((reference as f32) * p / 100.0).round() as usize,
        }
    }
}

/// Margin specification for overlays.
#[derive(Debug, Clone, Copy, Default)]
pub struct OverlayMargin {
    pub top: usize,
    pub right: usize,
    pub bottom: usize,
    pub left: usize,
}

impl OverlayMargin {
    /// Uniform margin on all sides.
    pub fn uniform(n: usize) -> Self {
        Self {
            top: n,
            right: n,
            bottom: n,
            left: n,
        }
    }
}

/// Options for overlay positioning and sizing.
pub struct OverlayOptions {
    /// Width of the overlay.
    pub width: Option<SizeValue>,
    /// Minimum width.
    pub min_width: Option<usize>,
    /// Maximum height.
    pub max_height: Option<SizeValue>,
    /// Anchor position (default: Center).
    pub anchor: OverlayAnchor,
    /// Horizontal offset from anchor.
    pub offset_x: i32,
    /// Vertical offset from anchor.
    pub offset_y: i32,
    /// Explicit row position (overrides anchor vertical).
    pub row: Option<SizeValue>,
    /// Explicit column position (overrides anchor horizontal).
    pub col: Option<SizeValue>,
    /// Margins around the overlay.
    pub margin: OverlayMargin,
    /// Dynamic visibility predicate. If set and returns false, the overlay is hidden.
    pub visible: Option<Box<dyn Fn(u16, u16) -> bool>>,
    /// If true, this overlay doesn't capture focus.
    pub non_capturing: bool,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            width: None,
            min_width: None,
            max_height: None,
            anchor: OverlayAnchor::Center,
            offset_x: 0,
            offset_y: 0,
            row: None,
            col: None,
            margin: OverlayMargin::default(),
            visible: None,
            non_capturing: false,
        }
    }
}

/// A resolved overlay layout (computed position and size).
struct OverlayLayout {
    row: usize,
    col: usize,
    width: usize,
    max_height: Option<usize>,
}

/// An entry in the overlay stack.
struct OverlayEntry {
    id: u64,
    component: Box<dyn Component>,
    options: OverlayOptions,
    hidden: bool,
    focus_order: u64,
    /// Whether this overlay is eligible for stack-order input routing
    /// when nothing else has explicit focus.
    ///
    /// For capturing overlays, this starts `true` (they enter the
    /// routing pool on show via [`Tui::show_overlay`]'s auto-focus
    /// path). Explicit [`Tui::unfocus_overlay`] removes them from the
    /// pool, so a user's "go away" request is honored even when the
    /// overlay is still on screen. Re-focusing with
    /// [`Tui::focus_overlay`] puts them back.
    ///
    /// Non-capturing overlays start `false`; they only join the pool
    /// when explicitly focused, and leave again on unfocus.
    routing_active: bool,
}

/// Handle to control a shown overlay.
pub struct OverlayHandle {
    id: u64,
}

impl OverlayHandle {
    /// Get the internal ID (used by TUI to find the entry).
    pub fn id(&self) -> u64 {
        self.id
    }
}

/// Where focus lives when it's not on an overlay. Used to remember the
/// previous focus target while an overlay has temporarily stolen it.
#[derive(Debug, Clone, Copy)]
enum FocusTarget {
    /// A root-child index.
    Child(usize),
    /// No focus was set before the overlay focus call.
    None,
}

// ---------------------------------------------------------------------------
// Cursor position extraction
// ---------------------------------------------------------------------------

/// How [`Tui::full_render`]'s `clear=true` path wipes the screen
/// before repainting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullClearMode {
    /// Clear the entire viewport, home the cursor to `(0, 0)`, and
    /// wipe the scrollback (`\x1b[2J\x1b[H\x1b[3J`). The home part is
    /// the critical bit: the subsequent repaint starts at an absolute
    /// physical row regardless of where `hardware_cursor_row` thought
    /// we were, so a full redraw recovers cleanly from any prior
    /// cursor-tracking drift (classically: scrolling past the bottom
    /// of the terminal between frames on a minimal vt100 that clamps
    /// `CUD`).
    ///
    /// Cost: any shell output above the TUI's working area is wiped
    /// along with the TUI content. This is the right tradeoff for a
    /// chat-style agent whose scrollback *is* the app, and the wrong
    /// one for a one-shot filter that wants to leave its caller's
    /// prior output visible above it.
    WholeScreen,
    /// Move to the tracked top of the rendered area and erase from
    /// the cursor downward (`\x1b[{n}A` + `\r` + `\x1b[J`). Preserves
    /// shell output above the TUI at the cost of assuming the tracked
    /// cursor row is correct — if it isn't, the wipe lands on the
    /// wrong rows, leaves stale content behind, or (worse) erases a
    /// row of shell output that should have been preserved.
    BelowCursor,
}

/// Extracted cursor position from rendered output.
struct CursorPosition {
    row: usize,
    col: usize,
}

// ---------------------------------------------------------------------------
// Crash logging
// ---------------------------------------------------------------------------

/// Resolve the path the renderer dumps a crash log to when an invariant
/// fails (most notably: a rendered line exceeds the terminal's width and
/// would corrupt the diff engine). Controlled by the `AJ_TUI_CRASH_LOG`
/// env var; falls back to `~/.aj/aj-tui-crash.log`. Returning `None`
/// means we couldn't determine any writeable path, in which case the
/// panic message below still carries the essentials.
fn resolve_crash_log_path() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("AJ_TUI_CRASH_LOG")
        && !value.is_empty()
    {
        return Some(PathBuf::from(value));
    }
    // `~/.aj/aj-tui-crash.log` is the project convention (see `AGENTS.md`:
    // "Persistent data lives in `~/.aj/`"). We create the directory on
    // demand in `write_crash_log` so the crash path works on a fresh
    // install that hasn't bootstrapped `~/.aj/` yet.
    home_dir().map(|h| h.join(".aj").join("aj-tui-crash.log"))
}

/// Local `home_dir` helper. `std::env::home_dir` was un-deprecated in
/// 1.86 but still carries the `deprecated` attribute on older toolchains
/// we want to build on; the `allow` keeps us compatible in both
/// directions without pulling in the `dirs` crate for one lookup.
fn home_dir() -> Option<PathBuf> {
    #[allow(deprecated)]
    {
        std::env::home_dir()
    }
}

/// Atomically (ish) dump a crash report to the resolved crash-log path.
/// Errors are swallowed: the caller is about to panic anyway and the
/// panic message is the backstop.
fn write_crash_log(header: &str, lines: &[String], width: usize) {
    let Some(path) = resolve_crash_log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut body = format!(
        "{header}\nTimestamp (unix): {ts}\nTerminal width: {width}\n\n=== All rendered lines ===\n"
    );
    for (i, line) in lines.iter().enumerate() {
        body.push_str(&format!("[{i}] (w={}) {line}\n", visible_width(line)));
    }
    body.push('\n');
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(body.as_bytes()));
}

// ---------------------------------------------------------------------------
// Render-decision debug log
// ---------------------------------------------------------------------------

/// Environment variable that, when set to a file path, enables per-render
/// decision-state logging. Captures the strategy the engine picked
/// (first-render / full-clear / diff / no-change), the row indices it
/// landed on, and the line contents of both the previous and new frames.
/// Complements [`crate::terminal::WRITE_LOG_ENV`] (which records the
/// emitted *bytes*) when a bug report is about *which* branch the
/// engine took, not whether the bytes it emitted were correct.
pub const DEBUG_LOG_ENV: &str = "AJ_TUI_DEBUG_LOG";

/// Return the debug-log path when [`DEBUG_LOG_ENV`] is set and
/// non-empty, else `None`. Empty/unset values disable logging — there's
/// no fall-through to a default path since the per-frame volume is
/// large enough that turning it on should be deliberate.
fn resolve_debug_log_path() -> Option<PathBuf> {
    let value = std::env::var(DEBUG_LOG_ENV).ok()?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

/// Record of the decisions `render` made on one frame. Populated during
/// rendering and flushed by [`Tui::render`] after the frame is emitted,
/// so the log reflects the engine's own view of the world — including
/// `hardware_cursor_row` *after* the render-strategy-specific code
/// updated it.
struct RenderDebugRecord {
    strategy: &'static str,
    cursor_at_before: usize,
    cursor_at_after: usize,
    first_changed: Option<usize>,
    last_changed: Option<usize>,
    prev_len: usize,
    new_len: usize,
    max_lines_rendered_before: usize,
    max_lines_rendered_after: usize,
    width: u16,
    height: u16,
    width_changed: bool,
    height_changed: bool,
    cursor_pos: Option<(usize, usize)>,
    prev_lines_snapshot: Vec<String>,
    new_lines_snapshot: Vec<String>,
}

impl RenderDebugRecord {
    fn append_to(&self, path: &Path) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let mut body = String::new();
        body.push_str(&format!(
            "--- render @ t={ts}ms ---\n\
             strategy: {strategy}\n\
             width: {w} (changed={wc})\n\
             height: {h} (changed={hc})\n\
             cursor_at: {cba} -> {cca}\n\
             cursor_pos: {cp}\n\
             first_changed: {fc}\n\
             last_changed: {lc}\n\
             prev_len: {pl}\n\
             new_len: {nl}\n\
             max_lines_rendered: {mlb} -> {mla}\n",
            strategy = self.strategy,
            w = self.width,
            wc = self.width_changed,
            h = self.height,
            hc = self.height_changed,
            cba = self.cursor_at_before,
            cca = self.cursor_at_after,
            cp = self
                .cursor_pos
                .map_or_else(|| "None".to_string(), |(r, c)| format!("({r}, {c})")),
            fc = self
                .first_changed
                .map_or_else(|| "-".to_string(), |v| v.to_string()),
            lc = self
                .last_changed
                .map_or_else(|| "-".to_string(), |v| v.to_string()),
            pl = self.prev_len,
            nl = self.new_len,
            mlb = self.max_lines_rendered_before,
            mla = self.max_lines_rendered_after,
        ));
        body.push_str("prev_lines:\n");
        for (i, line) in self.prev_lines_snapshot.iter().enumerate() {
            body.push_str(&format!("  [{i}] {line:?}\n"));
        }
        body.push_str("new_lines:\n");
        for (i, line) in self.new_lines_snapshot.iter().enumerate() {
            body.push_str(&format!("  [{i}] {line:?}\n"));
        }
        body.push('\n');
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(body.as_bytes()));
    }
}

/// Find and strip the CURSOR_MARKER from rendered lines.
/// Returns the cursor position if found.
///
/// Scans the visible viewport (the bottom `viewport_height` lines)
/// from the bottom up, matching pi-tui's `extractCursorPosition`
/// (`packages/tui/src/tui.ts:868`). Bottom-up is a deliberate perf
/// choice: the cursor lives in whatever component currently has
/// focus, which in chat/agent layouts is almost always an `Editor`
/// or `Input` pinned near the bottom of the frame. Iterating from
/// the bottom early-outs on the typical case before touching the
/// (often much longer) markdown / log content above it. See pi-tui
/// CHANGELOG #1004 for the original motivation.
///
/// Only the first marker occurrence on the chosen line is consumed:
/// the splice uses `marker_pos` directly so any additional markers
/// on the same line (or above) survive into the rendered frame. A
/// line with multiple markers is itself a component bug; matching
/// pi's single-splice behavior keeps downstream diagnostics honest
/// rather than silently scrubbing the extras.
fn extract_cursor_position(lines: &mut [String], viewport_height: usize) -> Option<CursorPosition> {
    // Only search the visible viewport (bottom `viewport_height` lines).
    let viewport_top = lines.len().saturating_sub(viewport_height);
    // Iterate row indices from `lines.len() - 1` down to `viewport_top`
    // (inclusive) — same shape as pi's `for (let row = lines.length -
    // 1; row >= viewportTop; row--)`. `(viewport_top..lines.len()).rev()`
    // is the idiomatic Rust spelling: empty when `lines` is empty
    // (no underflow), otherwise yields the bottom row first.
    for row in (viewport_top..lines.len()).rev() {
        let line = &mut lines[row];
        if let Some(marker_pos) = line.find(CURSOR_MARKER) {
            let before_marker = &line[..marker_pos];
            let col = visible_width(before_marker);
            // Strip exactly the marker we located. Using `replace` here
            // would scrub every occurrence on the line — we deliberately
            // splice out just the one at `marker_pos` so a stray second
            // marker stays visible in the frame for diagnosis.
            let mut spliced = String::with_capacity(line.len() - CURSOR_MARKER.len());
            spliced.push_str(&line[..marker_pos]);
            spliced.push_str(&line[marker_pos + CURSOR_MARKER.len()..]);
            *line = spliced;
            return Some(CursorPosition { row, col });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Overlay layout resolution
// ---------------------------------------------------------------------------

/// Whether an overlay entry is currently visible for a given terminal
/// size. An overlay is visible when its `hidden` flag is false and
/// either it has no `visible` predicate or the predicate returns true.
///
/// Shared by input routing, focus healing, and compositing so the three
/// surfaces can't drift apart on what "visible" means.
fn overlay_is_visible(entry: &OverlayEntry, cols: u16, rows: u16) -> bool {
    !entry.hidden
        && entry
            .options
            .visible
            .as_ref()
            .map(|f| f(cols, rows))
            .unwrap_or(true)
}

// Percent math and i32-clamped coordinate arithmetic both need `as`
// casts that no stable safe wrapper covers (there's no `usize → f32`
// or `f32 → usize` in `From`/`TryFrom`, and the i32↔usize round trips
// here are guarded by `clamp` against `margin.top` / `margin.left` so
// they can't go negative). Terminal dimensions stay well below
// `i32::MAX` and the `f32` precision threshold (2^24) in practice.
#[allow(clippy::as_conversions)]
fn resolve_overlay_layout(
    opts: &OverlayOptions,
    content_height: usize,
    term_width: usize,
    term_height: usize,
) -> OverlayLayout {
    let margin = &opts.margin;
    // Available space inside the margins, with a minimum of 1 so a
    // pathological "margin equals or exceeds terminal size" config
    // still resolves to a single-cell area instead of zero (and the
    // final clamp below floats the overlay against `margin.top` /
    // `margin.left`).
    let avail_width = term_width.saturating_sub(margin.left + margin.right).max(1);
    let avail_height = term_height
        .saturating_sub(margin.top + margin.bottom)
        .max(1);

    // Width.
    let mut width = opts
        .width
        .map(|sv| sv.resolve(term_width))
        .unwrap_or_else(|| avail_width.min(80));
    if let Some(min_w) = opts.min_width {
        width = width.max(min_w);
    }
    width = width.min(avail_width).max(1);

    // Max height.
    let max_height = opts
        .max_height
        .map(|sv| sv.resolve(term_height).min(avail_height).max(1));

    // The caller (`composite_overlays`) does a two-pass layout: a
    // first pass with `content_height = 0` to discover `width` and
    // `max_height`, then a second pass with the rendered overlay's
    // (max-height-clamped) line count. So `content_height` here is
    // already the effective height we want to use for position math.
    let effective_height = content_height;

    // Row position.
    let row: i32 = if let Some(ref rv) = opts.row {
        match rv {
            // Absolute rows pass through unchanged. The final clamp
            // (below) is what keeps them inside the terminal.
            SizeValue::Absolute(n) => *n as i32,
            // Percentage rows are *position-aware*: 0% = top of
            // available area, 100% = far enough down that the whole
            // overlay still fits, with linear interpolation between.
            // This matches the intuition "row: 50% means vertically
            // centered" rather than "row: 50% means top edge halfway
            // down the terminal".
            SizeValue::Percent(p) => {
                let max_row = avail_height.saturating_sub(effective_height);
                margin.top as i32 + ((max_row as f32) * p / 100.0).floor() as i32
            }
        }
    } else {
        (resolve_anchor_row(opts.anchor, effective_height, avail_height) + margin.top) as i32
    };

    // Column position.
    let col: i32 = if let Some(ref cv) = opts.col {
        match cv {
            SizeValue::Absolute(n) => *n as i32,
            SizeValue::Percent(p) => {
                let max_col = avail_width.saturating_sub(width);
                margin.left as i32 + ((max_col as f32) * p / 100.0).floor() as i32
            }
        }
    } else {
        (resolve_anchor_col(opts.anchor, width, avail_width) + margin.left) as i32
    };

    // Apply offsets.
    let row = row + opts.offset_y;
    let col = col + opts.offset_x;

    // Clamp to terminal bounds, respecting margins. The final clamp
    // is `max(margin, min(value, term - margin - effective))`. When
    // the upper bound falls below the margin (overlay can't fit even
    // pinned to the top/left margin), we float against the margin
    // rather than computing a negative row.
    let row_upper = (term_height.saturating_sub(margin.bottom + effective_height)) as i32;
    let col_upper = (term_width.saturating_sub(margin.right + width)) as i32;
    let row_upper = row_upper.max(margin.top as i32);
    let col_upper = col_upper.max(margin.left as i32);
    let row = row.clamp(margin.top as i32, row_upper) as usize;
    let col = col.clamp(margin.left as i32, col_upper) as usize;

    OverlayLayout {
        row,
        col,
        width,
        max_height,
    }
}

fn resolve_anchor_row(anchor: OverlayAnchor, content_height: usize, avail_height: usize) -> usize {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::TopRight | OverlayAnchor::TopCenter => 0,
        OverlayAnchor::BottomLeft | OverlayAnchor::BottomRight | OverlayAnchor::BottomCenter => {
            avail_height.saturating_sub(content_height)
        }
        OverlayAnchor::Center | OverlayAnchor::LeftCenter | OverlayAnchor::RightCenter => {
            avail_height.saturating_sub(content_height) / 2
        }
    }
}

fn resolve_anchor_col(anchor: OverlayAnchor, width: usize, avail_width: usize) -> usize {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::BottomLeft | OverlayAnchor::LeftCenter => 0,
        OverlayAnchor::TopRight | OverlayAnchor::BottomRight | OverlayAnchor::RightCenter => {
            avail_width.saturating_sub(width)
        }
        OverlayAnchor::Center | OverlayAnchor::TopCenter | OverlayAnchor::BottomCenter => {
            avail_width.saturating_sub(width) / 2
        }
    }
}

// ---------------------------------------------------------------------------
// Line compositing
// ---------------------------------------------------------------------------

/// Composite an overlay line onto a base line at a specific column.
///
/// Handles three boundary concerns beyond the naive "splice segments
/// together" recipe:
///
/// 1. The overlay is truncated with [`slice_with_width`] (`strict = true`)
///    before compositing. A wide grapheme whose left half fits at
///    `overlay_width - 1` but whose right half would extend to
///    `overlay_width + 1` is dropped; otherwise the composited line's
///    visible width would exceed the overlay's declared width and
///    trample the "after" segment's columns.
///
/// 2. The result is padded to `total_width` via right-side
///    (`after_padding`) spaces so the composited row always has the
///    same visible width as the terminal. Without this, a row whose
///    base content stopped short of `total_width` would leave the
///    right-hand cells with whatever stale content was already on the
///    terminal, and the diff engine's byte-equality check would see
///    the short row as equal to its predecessor and skip re-emitting
///    it — the stale cells would persist indefinitely.
///
/// 3. A post-composition width clamp truncates the final line to
///    `total_width`. Style bytes in the input (OSC 8, SGR, etc.) can
///    produce enough edge cases in the segment extraction that the
///    combined output occasionally drifts past the terminal width by
///    one or two columns; if it does, we re-slice the result with
///    `strict = true` rather than letting the oversize line reach the
///    render engine's phase-4.5 sanity check (which would panic).
fn composite_line_at(
    base: &str,
    overlay: &str,
    start_col: usize,
    overlay_width: usize,
    total_width: usize,
) -> String {
    let (before, before_width, after, after_width) = extract_segments(
        base,
        start_col,
        start_col + overlay_width,
        total_width.saturating_sub(start_col + overlay_width),
        true,
    );

    // Truncate the overlay to its declared width with strict boundary
    // handling. Callers are expected to respect the width they were
    // given, but this is the final safeguard against a wide grapheme
    // at the overlay boundary leaking into the "after" segment.
    let (overlay_truncated, overlay_vis) = slice_with_width(overlay, 0, overlay_width, true);

    // Compute the visible footprint of each segment using
    // `max(declared, actual)`. `before` or `overlay` can legitimately
    // overshoot their declared widths when a wide grapheme sits at the
    // boundary and strict slicing includes it; the after-segment
    // budget must accommodate that, otherwise `after_target` underflows
    // and the right-side padding ends up negative-clamped-to-zero when
    // we actually want to fill cells.
    let actual_before_width = start_col.max(before_width);
    let actual_overlay_width = overlay_width.max(overlay_vis);
    let after_target = total_width.saturating_sub(actual_before_width + actual_overlay_width);

    let before_padding = start_col.saturating_sub(before_width);
    let overlay_padding = overlay_width.saturating_sub(overlay_vis);
    let after_padding = after_target.saturating_sub(after_width);

    let mut result = String::new();

    // Before segment, padded to start_col.
    result.push_str(&before);
    for _ in 0..before_padding {
        result.push(' ');
    }
    result.push_str(SEGMENT_RESET);

    // Overlay content.
    result.push_str(&overlay_truncated);
    for _ in 0..overlay_padding {
        result.push(' ');
    }
    result.push_str(SEGMENT_RESET);

    // After segment, padded out to `total_width`. Without this
    // padding, a composited row whose base content was shorter than
    // the terminal width leaves the right-hand cells untouched by
    // this frame — and on the next differential render, the
    // render-engine's string-equality check would see the truncated
    // row as equal to itself and skip re-emitting it, leaving the
    // stale cells visible. The right-side pad keeps the composited
    // row a fixed `total_width` visible cells wide.
    result.push_str(&after);
    for _ in 0..after_padding {
        result.push(' ');
    }

    // Last-line defense: if the composed result has drifted past
    // `total_width` (because the overlay contained complex escape
    // sequences whose segment math nudged the tracker off by a
    // column), re-slice with `strict = true` to bring it back in
    // bounds. The render engine's phase-4.5 check would otherwise
    // panic on the overwide frame.
    let result_width = visible_width(&result);
    if result_width <= total_width {
        return result;
    }
    slice_by_column(&result, 0, total_width, true)
}

// ---------------------------------------------------------------------------
// Input listeners
// ---------------------------------------------------------------------------

/// What a registered [`InputListener`] wants the `Tui` to do with the event
/// it just saw.
///
/// Listeners run in insertion order before any overlay or focus routing, so
/// they act as a pre-component interception hook: rewrite an event, swallow
/// it entirely, or pass it through unchanged.
pub enum InputListenerAction {
    /// Pass the original event through to the next listener (or to the
    /// dispatch logic, if this is the last listener).
    Pass,
    /// Replace the event seen by subsequent listeners and the dispatch
    /// logic.
    Rewrite(InputEvent),
    /// Stop dispatch entirely. No further listeners run and the event is
    /// not delivered to any component.
    Consume,
}

/// Pre-component input interception hook. The boxed closure is called with
/// the current event (possibly rewritten by an earlier listener) on every
/// [`Tui::handle_input`] call, before routing kicks in. Registered via
/// [`Tui::add_input_listener`] and removed via [`Tui::remove_input_listener`].
type InputListener = Box<dyn FnMut(&InputEvent) -> InputListenerAction>;

/// Handle returned by [`Tui::add_input_listener`] and consumed by
/// [`Tui::remove_input_listener`]. Opaque identifier that's safe to store
/// without leaking the listener's storage representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputListenerHandle(u64);

/// Registered-listener slot. The option lets us tombstone a listener if
/// removal happens from inside the dispatch loop (none of the current
/// callers do, but the shape is cheap and forward-compatible).
struct InputListenerSlot {
    id: u64,
    listener: InputListener,
}

// ---------------------------------------------------------------------------
// TUI engine
// ---------------------------------------------------------------------------

/// The main TUI rendering engine.
///
/// Manages a tree of components, an overlay stack, focus, and a differential
/// rendering loop that outputs only changed lines.
pub struct Tui {
    /// The root container holding base content. Private; the public
    /// surface for child management is the forwarding methods on
    /// `Tui` itself ([`Tui::add_child`], [`Tui::remove_child_by_ref`],
    /// etc.), which mirror pi-tui's `TUI extends Container` shape.
    root: Container,

    // Terminal.
    terminal: Box<dyn Terminal>,

    // Render state.
    previous_lines: Vec<String>,
    previous_width: u16,
    previous_height: u16,
    hardware_cursor_row: usize,
    max_lines_rendered: usize,
    /// Logical row number currently at physical row 0 of the visible
    /// viewport. When the TUI area fits in the terminal, this is 0;
    /// once it has scrolled (because the engine wrote past the bottom
    /// row and the terminal shifted everything up), `previous_viewport_top`
    /// advances by the number of rows scrolled. Used for two things:
    ///
    /// 1. Deciding when a diff render must fall back to full redraw —
    ///    if the first changed line is above `previous_viewport_top`
    ///    (i.e. off-screen in scrollback), the diff path physically
    ///    can't reach it.
    /// 2. Tracking the physical-cursor/logical-row mapping across
    ///    renders so `hardware_cursor_row` stays meaningful even
    ///    after scrolls.
    ///
    /// Tracked best-effort in [`FullClearMode::BelowCursor`] mode
    /// (shell output above the TUI makes the mapping approximate);
    /// the [`FullClearMode::WholeScreen`] default gives exact
    /// tracking because the TUI owns the entire viewport after every
    /// full redraw.
    previous_viewport_top: usize,
    last_render_time: Option<StdInstant>,
    render_requested: bool,
    /// Set by [`Tui::force_full_render`] / [`Tui::request_full_render`]
    /// to flag that the next `render` call must clear the screen before
    /// repainting, even if the engine would otherwise have taken the
    /// first-render path. Cleared after the full-clear render runs.
    ///
    /// Without this, resetting `previous_lines` and `previous_width` to
    /// their pristine values makes the engine think it's rendering for
    /// the first time, and first-render takes the `clear=false` branch
    /// (to preserve any shell output above). A caller that explicitly
    /// asks for a full re-render usually wants the screen wiped, so we
    /// need an extra signal to distinguish a genuine first render from
    /// a recovery render. Upstream achieves the same effect with -1
    /// sentinels on the previous dimensions that coerce the width-
    /// changed branch to fire; a dedicated flag is friendlier to type
    /// systems that don't have cheap out-of-band sentinel values.
    pending_full_clear: bool,

    // Observability counters (used by integration tests to assert rendering
    // strategy without scraping terminal output).
    full_redraws: u64,
    /// Total number of `render()` calls that reached the strategy-
    /// dispatch stage (i.e. terminal had non-zero dimensions). Includes
    /// full redraws, differential renders, and no-change renders —
    /// any call that ran the strategy selector. Used by async
    /// coalescing tests that want to assert "exactly one render ran"
    /// without depending on throttle timing.
    total_renders: u64,

    // Behavior flags.
    clear_on_shrink: bool,
    strict_line_widths: bool,
    full_clear_mode: FullClearMode,

    // Focus.
    focused_component_index: Option<usize>,
    /// If set, input is routed here first (even for non-capturing
    /// overlays) so an explicit `focus_overlay` call wins over normal
    /// stack-order routing.
    focused_overlay_id: Option<u64>,
    /// Focus state saved when an overlay steals focus via
    /// [`Tui::focus_overlay`] so [`Tui::unfocus_overlay`] can restore it.
    saved_focus: Option<FocusTarget>,

    // Overlays.
    overlays: Vec<OverlayEntry>,
    next_overlay_id: u64,
    next_focus_order: u64,

    // Input listeners (pre-component hooks).
    input_listeners: Vec<InputListenerSlot>,
    next_listener_id: u64,

    /// Global debug hook fired before input routing when the user
    /// presses `Shift+Ctrl+D`. Intended as a dev-time entry point —
    /// apps that register a callback here typically use it to dump
    /// internal state, toggle a diagnostic overlay, or tee stats out
    /// to a file. The callback runs before any overlay or focused
    /// component sees the event, and once it returns the event is
    /// still consumed (not forwarded) — matching the convention that
    /// a dedicated debug chord shouldn't double-fire as a component
    /// input.
    on_debug: Option<Box<dyn FnMut()>>,

    // Hardware cursor display.
    //
    // `hardware_cursor_enabled` is the *user preference*: does the app want
    // a real terminal cursor shown at the `CURSOR_MARKER` position, or
    // should the inline marker that focus-aware components embed be the
    // only cursor indication? Default `true` so focus-aware components
    // (editor, text input) behave as most apps expect; set to `false`
    // globally (for example in a status-display TUI with no text input)
    // to keep the cursor hidden regardless of marker placement.
    //
    // `hardware_cursor_currently_shown` is the *state* — whether the last
    // escape sequence we emitted left the cursor visible. Used only to
    // avoid redundant `\x1b[?25h` / `\x1b[?25l` emissions on consecutive
    // renders (some terminals briefly flash the cursor each time `?25h`
    // arrives, which is visible as flicker when a popup stays open for
    // many keystrokes).
    hardware_cursor_enabled: bool,
    hardware_cursor_currently_shown: bool,

    /// Set once [`Tui::stop`] has completed successfully. Used to keep
    /// `stop` idempotent so calling it explicitly before drop (the
    /// recommended pattern) doesn't emit a second terminal-restore
    /// sequence from the `Drop` impl.
    stopped: bool,

    // -- Async event-loop machinery. --
    /// Cloneable sender end of the render-request channel. Handed out
    /// to async tasks via [`Tui::handle`] so they can wake the event
    /// loop without taking a reference to the `Tui` itself.
    render_tx: mpsc::UnboundedSender<()>,
    /// Receiver end of the render-request channel. Polled inside
    /// [`Tui::next_event`] to notice external render requests.
    render_rx: mpsc::UnboundedReceiver<()>,
    /// The terminal's input stream, taken during [`Tui::start`]. `None`
    /// before `start` (or after input has ended).
    input_stream: Option<InputStream>,
    /// Throttle timer. Initialized lazily on first [`Tui::next_event`]
    /// call so tests that never poll the event loop don't pay for the
    /// timer subscription.
    throttle: Option<Interval>,
    /// Minimum interval between [`TuiEvent::Render`] emissions.
    render_interval: Duration,
    /// If true, [`Tui::next_event`] yields a [`TuiEvent::Render`] as
    /// soon as the throttle fires, without requiring an explicit
    /// [`Tui::request_render`] or [`RenderHandle::request_render`]
    /// call. This makes the first frame appear without bootstrap
    /// ceremony.
    initial_render: bool,

    /// Most recent terminal dimensions, shared with every
    /// [`RenderHandle`] this `Tui` mints. Updated in [`Tui::start`]
    /// (so components see real values before the first render) and
    /// at the top of [`Tui::render`] (to track resizes). Components
    /// that need to size themselves to the terminal — for example
    /// the [`crate::components::editor::Editor`]'s auto-sized scroll
    /// window — read from these via the handle.
    term_rows: Arc<AtomicU16>,
    term_cols: Arc<AtomicU16>,
}

impl Tui {
    /// Create a new TUI with the given terminal backend.
    ///
    /// Infallible: terminal setup (raw mode, bracketed paste, cursor
    /// hide, input-stream take) happens later in [`Tui::start`].
    /// Components can be added via [`Tui::add_child`] and [`RenderHandle`]s
    /// can be minted via [`Tui::handle`] before `start` is called.
    pub fn new(terminal: Box<dyn Terminal>) -> Self {
        let (render_tx, render_rx) = mpsc::unbounded_channel::<()>();
        let term_rows = Arc::new(AtomicU16::new(0));
        let term_cols = Arc::new(AtomicU16::new(0));
        let root = Container::new();
        Self {
            root,
            terminal,
            previous_lines: Vec::new(),
            previous_width: 0,
            previous_height: 0,
            hardware_cursor_row: 0,
            max_lines_rendered: 0,
            previous_viewport_top: 0,
            last_render_time: None,
            render_requested: false,
            pending_full_clear: false,
            full_redraws: 0,
            total_renders: 0,
            clear_on_shrink: false,
            strict_line_widths: true,
            full_clear_mode: FullClearMode::WholeScreen,
            focused_component_index: None,
            focused_overlay_id: None,
            saved_focus: None,
            overlays: Vec::new(),
            next_overlay_id: 1,
            next_focus_order: 1,
            input_listeners: Vec::new(),
            next_listener_id: 1,
            on_debug: None,
            hardware_cursor_enabled: true,
            hardware_cursor_currently_shown: false,
            stopped: false,
            render_tx,
            render_rx,
            input_stream: None,
            throttle: None,
            render_interval: MIN_RENDER_INTERVAL,
            initial_render: true,
            term_rows,
            term_cols,
        }
    }

    /// Cloneable handle for requesting renders from async tasks.
    ///
    /// Each clone is cheap (it wraps an [`mpsc::UnboundedSender`]) and
    /// can be freely moved across tasks. The handle is valid for the
    /// lifetime of the process even if the `Tui` is later dropped; in
    /// that case further `request_render` calls become no-ops.
    pub fn handle(&self) -> RenderHandle {
        RenderHandle {
            tx: self.render_tx.clone(),
            term_rows: Arc::clone(&self.term_rows),
            term_cols: Arc::clone(&self.term_cols),
        }
    }

    /// Override the minimum interval between [`TuiEvent::Render`]
    /// emissions. Must be called before [`Tui::next_event`] so the
    /// throttle picks it up; later adjustments require a restart of
    /// the event loop.
    pub fn set_render_interval(&mut self, interval: Duration) {
        self.render_interval = interval;
        // Invalidate any throttle that was built with the old interval
        // so the next `next_event` rebuilds it.
        self.throttle = None;
    }

    /// Disable (or re-enable) the implicit initial render.
    ///
    /// By default the first [`Tui::next_event`] call schedules a
    /// [`TuiEvent::Render`] so applications don't have to bootstrap the
    /// first frame. Tests that want deterministic control over render
    /// events should disable this.
    pub fn set_initial_render(&mut self, initial: bool) {
        self.initial_render = initial;
    }

    /// Get a reference to the terminal backend.
    pub fn terminal(&self) -> &dyn Terminal {
        self.terminal.as_ref()
    }

    /// Get a mutable reference to the terminal backend.
    pub fn terminal_mut(&mut self) -> &mut dyn Terminal {
        self.terminal.as_mut()
    }

    // -----------------------------------------------------------------
    // Container forwarding surface
    //
    // Pi-tui's `TUI extends Container`, so callers reach the child list
    // directly on the `tui` instance (`tui.addChild(c)`, `tui.children[i]`,
    // `tui.removeChild(c)`). Rust can't `extend` a struct, but the same
    // ergonomic shape falls out of thin pass-through methods on `Tui`
    // that delegate to the private `root: Container`. Behavior is
    // identical to calling `self.root.X(...)`; see [`Container`] for
    // semantics.
    //
    // We deliberately *don't* `impl Deref<Target = Container> for Tui`
    // to get this for free: `Component` methods on `Container` would
    // then become silently reachable on a `Tui` value, which is a
    // footgun for the trait methods that `Tui` overrides with
    // engine-aware behavior (notably [`Tui::invalidate`], which walks
    // overlays in addition to the root). Hand-rolled forwarders keep
    // the surface explicit and make those overrides authoritative.
    // -----------------------------------------------------------------

    /// Append `child` to the root container's children list.
    /// Forwards to [`Container::add_child`].
    pub fn add_child(&mut self, child: Box<dyn Component>) {
        self.root.add_child(child);
    }

    /// Insert `child` into the root container at `index`.
    /// Forwards to [`Container::insert_child`].
    pub fn insert_child(&mut self, index: usize, child: Box<dyn Component>) {
        self.root.insert_child(index, child);
    }

    /// Remove the first root-container child whose identity matches
    /// `child`. Forwards to [`Container::remove_child_by_ref`].
    pub fn remove_child_by_ref(&mut self, child: &dyn Component) -> Option<Box<dyn Component>> {
        self.root.remove_child_by_ref(child)
    }

    /// Drop every child in the root container.
    /// Forwards to [`Container::clear`].
    pub fn clear(&mut self) {
        self.root.clear();
    }

    /// Number of children in the root container.
    /// Forwards to [`Container::len`].
    pub fn len(&self) -> usize {
        self.root.len()
    }

    /// True when the root container has no children.
    /// Forwards to [`Container::is_empty`].
    pub fn is_empty(&self) -> bool {
        self.root.is_empty()
    }

    /// Index of the last child in the root container, or `None` when
    /// empty. Forwards to [`Container::last_index`].
    pub fn last_index(&self) -> Option<usize> {
        self.root.last_index()
    }

    /// Borrow the root-container child at `index`.
    /// Forwards to [`Container::get`].
    pub fn get(&self, index: usize) -> Option<&dyn Component> {
        self.root.get(index)
    }

    /// Mutably borrow the root-container child at `index`.
    /// Forwards to [`Container::get_mut`].
    pub fn get_mut(&mut self, index: usize) -> Option<&mut Box<dyn Component>> {
        self.root.get_mut(index)
    }

    /// Borrow the root-container child at `index` downcast to `T`.
    /// Forwards to [`Container::get_as`].
    pub fn get_as<T: Component>(&self, index: usize) -> Option<&T> {
        self.root.get_as::<T>(index)
    }

    /// Mutably borrow the root-container child at `index` downcast to
    /// `T`. Forwards to [`Container::get_mut_as`].
    pub fn get_mut_as<T: Component>(&mut self, index: usize) -> Option<&mut T> {
        self.root.get_mut_as::<T>(index)
    }

    /// Invalidate every component the engine owns: each root-container
    /// child and each overlay (visible or hidden). Mirrors pi-tui's
    /// `TUI.invalidate` override, which extends `Container.invalidate`
    /// to also walk the overlay stack so a global event (theme change,
    /// resize, palette swap) reaches everything that might cache
    /// rendered output.
    ///
    /// Hidden overlays are invalidated too: an overlay can be hidden
    /// and later re-shown with [`OverlayHandle`]-style controls, and
    /// re-showing must not display stale cached lines from before the
    /// invalidating event.
    pub fn invalidate(&mut self) {
        self.root.invalidate();
        for overlay in &mut self.overlays {
            overlay.component.invalidate();
        }
    }

    /// Show an overlay with the given component and options.
    /// Returns a handle that can be used to hide or manipulate the overlay.
    ///
    /// Capturing overlays (the default, `non_capturing: false`) auto-focus
    /// on show, matching the intuition that a modal grabs input until
    /// dismissed. Non-capturing overlays are shown without stealing focus;
    /// applications promote them into the input path with
    /// [`Tui::focus_overlay`].
    pub fn show_overlay(
        &mut self,
        component: Box<dyn Component>,
        options: OverlayOptions,
    ) -> OverlayHandle {
        let id = self.next_overlay_id;
        self.next_overlay_id += 1;
        let focus_order = self.next_focus_order;
        self.next_focus_order += 1;
        let non_capturing = options.non_capturing;

        // Overlay components are constructed with their own
        // [`RenderHandle`] (any component that needs one — Editor,
        // Loader, CancellableLoader — takes it as a required
        // constructor arg). The Tui no longer injects one on show;
        // each overlay either already has the handle it needs or has
        // no async work to wake.

        self.overlays.push(OverlayEntry {
            id,
            component,
            options,
            hidden: false,
            focus_order,
            routing_active: !non_capturing,
        });

        let handle = OverlayHandle { id };
        if !non_capturing {
            // Capturing overlays auto-focus on show; focus_overlay also
            // saves the prior focus target so hide/unfocus can restore
            // it.
            self.focus_overlay(&handle);
        }
        handle
    }

    /// Hide and remove an overlay by its handle.
    ///
    /// If the overlay currently has focus (explicit via [`Tui::focus_overlay`]
    /// or implicit via a capturing auto-focus), removing it promotes
    /// focus to the next-topmost visible capturing overlay. If no such
    /// overlay remains, focus falls back to whatever was focused before
    /// any overlays were shown (the saved pre-focus). This matches the
    /// intuition that dismissing a modal reveals whatever was behind it.
    pub fn hide_overlay(&mut self, handle: &OverlayHandle) {
        let id = handle.id();
        let was_focused = self.focused_overlay_id == Some(id);

        // Clear overlay focus state on the entry before removing.
        if let Some(entry) = self.overlays.iter_mut().find(|e| e.id == id) {
            entry.component.set_focused(false);
        }
        self.overlays.retain(|e| e.id != id);

        if was_focused {
            self.focused_overlay_id = None;
            self.promote_focus_after_unfocus();
        }
    }

    /// Transfer focus to the next-topmost visible capturing overlay, or
    /// restore the saved pre-focus target if none exist. Used by the
    /// overlay-focus-loss paths (`hide_overlay`, `unfocus_overlay`,
    /// `set_overlay_hidden`) so they behave consistently.
    fn promote_focus_after_unfocus(&mut self) {
        // Only overlays that are still in the routing pool (i.e., not
        // previously unfocused) are promotion candidates. This keeps a
        // closed overlay below a currently-hidden focused one from
        // silently re-entering the input path.
        let promote = self
            .overlays
            .iter()
            .filter(|e| e.routing_active && !e.hidden && !e.options.non_capturing)
            .max_by_key(|e| e.focus_order)
            .map(|e| e.id);

        if let Some(promote_id) = promote {
            // Inline: don't re-enter focus_overlay (which would also
            // overwrite saved_focus).
            self.focused_overlay_id = Some(promote_id);
            let next_order = self.next_focus_order;
            self.next_focus_order += 1;
            if let Some(entry) = self.overlays.iter_mut().find(|e| e.id == promote_id) {
                entry.component.set_focused(true);
                entry.focus_order = next_order;
                entry.routing_active = true;
            }
            self.request_render();
            return;
        }

        // No capturing overlay to promote: restore the pre-focus target.
        if let Some(saved) = self.saved_focus.take() {
            match saved {
                FocusTarget::Child(idx) => {
                    self.focused_component_index = Some(idx);
                    if let Some(child) = self.root.get_mut(idx) {
                        child.set_focused(true);
                    }
                }
                FocusTarget::None => {
                    self.focused_component_index = None;
                }
            }
        }
        self.request_render();
    }

    /// Hide and remove the topmost overlay, but only if it is a capturing
    /// overlay. When the topmost overlay (by render z-order) is non-
    /// capturing, the call is a no-op: non-capturing overlays are
    /// backdrop layers and shouldn't be dismissed by an Esc handler
    /// targeting the topmost modal.
    ///
    /// Returns `true` if an overlay was hidden.
    pub fn hide_topmost_overlay(&mut self) -> bool {
        let topmost = self
            .overlays
            .iter()
            .filter(|e| !e.hidden)
            .max_by_key(|e| e.focus_order);
        let Some(entry) = topmost else {
            return false;
        };
        if entry.options.non_capturing {
            return false;
        }
        let handle = OverlayHandle { id: entry.id };
        self.hide_overlay(&handle);
        true
    }

    /// Render immediately. Call this in your event loop after processing events.
    pub fn render(&mut self) {
        let width = usize::from(self.terminal.columns());
        let height = usize::from(self.terminal.rows());

        // Republish dimensions to every outstanding [`RenderHandle`]
        // so consumers (e.g. the editor's auto-sized scroll window)
        // observe the latest size on every frame, including resizes.
        // Done before the early-return so a 0-sized terminal still
        // updates handles (handle consumers handle 0 explicitly).
        self.term_rows
            .store(self.terminal.rows(), Ordering::Relaxed);
        self.term_cols
            .store(self.terminal.columns(), Ordering::Relaxed);

        if width == 0 || height == 0 {
            return;
        }

        // Observability: count every render that reached the actual
        // painting phase. `full_redraws` stays as the narrower
        // "how many full redraws happened" counter; `total_renders`
        // lets async tests assert on coalescing independent of
        // strategy.
        self.total_renders += 1;

        // Phase 1: Render content. Components that maintain async
        // state (most notably the editor's autocomplete pipeline)
        // drain their result channels at the top of their own
        // `render(&mut self, …)` — no framework-wide tick hook needed.
        let mut lines = self.root.render(width);

        // Phase 2: Composite overlays.
        self.composite_overlays(&mut lines, width, height);

        // Phase 3: Extract cursor position.
        let cursor_pos = extract_cursor_position(&mut lines, height);

        // Phase 4: Apply line resets. Every line gets the full
        // `SEGMENT_RESET` terminator (SGR reset + OSC 8 empty URL), not
        // just `SGR_RESET`. A plain SGR reset closes colors and attributes
        // but leaves any open hyperlink dangling — if a component emits
        // `\x1b]8;;https://x\x1b\\label` without a matching
        // `\x1b]8;;\x1b\\` closer, the URL attribute bleeds into every
        // subsequent row until something else happens to terminate it.
        // Appending `SEGMENT_RESET` lets component authors omit the
        // hyperlink close sequence without contaminating the rest of
        // the frame.
        //
        // Empty lines get the reset too. A row produced as `""` by a
        // component sits below a styled row whose terminator just closed
        // its own SGR state — but on a degraded sync-mode setup (BSU not
        // honored end-to-end), an in-progress style on the *terminal*
        // (not in our line buffer) can bleed into the empty row's cells
        // until something resets. The reset on an otherwise-empty line
        // is a two-byte safety net against that bleed-through.
        for line in &mut lines {
            line.push_str(SEGMENT_RESET);
        }

        // Phase 4.5: Sanity-check line widths. A component that emits a
        // line wider than the terminal throws off every downstream width
        // calculation — the diff engine misplaces cursor moves, the
        // wrap math in components that expect pre-wrapped input melts
        // down, and the net effect in the user's terminal is either
        // duplicated rows or a hard hang on the next keystroke. We'd
        // rather halt cleanly with a diagnostic than keep painting over
        // a corrupted frame.
        self.validate_line_widths(&lines);

        // Phase 5: Determine rendering strategy and emit output.
        let current_width = self.terminal.columns();
        let current_height = self.terminal.rows();
        let width_changed = current_width != self.previous_width;
        let height_changed = current_height != self.previous_height;
        let first_render = self.previous_lines.is_empty() && self.max_lines_rendered == 0;
        // clear-on-shrink compares the new render against the *historical
        // high-water mark* of rendered lines (`max_lines_rendered`), not
        // just the previous render's length. Tracking the high-water
        // mark matters when a transient component — a selector
        // dropdown, a tool-call log that scrolled away, anything that
        // briefly grew the working area — is dismissed and leaves
        // behind rows that, from the diff engine's point of view, were
        // already cleared by earlier renders but that we still want to
        // re-check before a steady-state render. The overlay-stack
        // guard exists because an active overlay inflates `lines.len()`
        // for composition reasons, and triggering a full redraw every
        // time an overlay is dismissed produces jumpy output.
        let shrunk = !first_render
            && self.clear_on_shrink
            && self.overlays.is_empty()
            && lines.len() < self.max_lines_rendered;

        // Height changes normally need a full redraw to realign the viewport.
        // Termux toggles height whenever its software keyboard shows/hides,
        // so a full redraw there causes every toggle to replay the history.
        let height_forces_full_redraw =
            height_changed && !first_render && !should_skip_full_redraw_on_height_change();

        // Best-effort viewport-top recompute when a height change takes
        // the diff path (i.e. Termux or any caller that suppressed the
        // full-redraw-on-height-change behavior). The stored
        // `previous_viewport_top` is relative to the *old* height; once
        // the terminal has a new height, the visible window shifts and
        // the tracker needs to follow, otherwise `diff_above_viewport`
        // and `deletion_only_needs_full` measure against stale
        // coordinates.
        //
        // Upstream computes:
        //   previousBufferLength = prevViewportTop + prevHeight
        //   prevViewportTop = max(0, previousBufferLength - newHeight)
        //
        // The intuition: the "buffer length" is how many logical rows
        // the previous frame spanned (viewport top + viewport height).
        // The new viewport top is that buffer length minus the new
        // height — i.e. anchor the bottom of the buffer to the bottom
        // of the new viewport, matching the render engine's bottom-
        // aligned layout.
        let effective_viewport_top = if height_changed
            && !first_render
            && !height_forces_full_redraw
            && self.previous_height > 0
        {
            let prev_buffer_len = self.previous_viewport_top + usize::from(self.previous_height);
            prev_buffer_len.saturating_sub(usize::from(current_height))
        } else {
            self.previous_viewport_top
        };

        // Debug-log setup: capture pre-render state so we can correlate
        // the decision with the branch taken, regardless of which branch
        // writes to `hardware_cursor_row` first. Cheap when the log
        // isn't enabled (one env-var lookup).
        let debug_log_path = resolve_debug_log_path();
        let cursor_at_before = self.hardware_cursor_row;
        let max_lines_rendered_before = self.max_lines_rendered;
        let prev_lines_snapshot: Vec<String> = if debug_log_path.is_some() {
            self.previous_lines.clone()
        } else {
            Vec::new()
        };
        let new_lines_snapshot: Vec<String> = if debug_log_path.is_some() {
            lines.clone()
        } else {
            Vec::new()
        };

        // Peek the diff range *before* picking a render strategy so we
        // can detect two fallback cases the diff path can't handle:
        //
        // - `diff_above_viewport`: the first changed row is above the
        //   current viewport top (i.e. scrolled off into scrollback).
        //   No amount of `\x1b[nA` or `\r\n` can rewrite a row that
        //   lives above physical row 0.
        // - `deletion_only_needs_full`: the frame shrunk enough that
        //   every change is in rows past the end of the new frame AND
        //   either the new end-of-content lives above the current
        //   viewport top (we'd be clearing rows that are off-screen,
        //   which corrupts tracking) or the number of rows to clear
        //   exceeds the terminal height (our `\x1b[1B`-based cleanup
        //   loop assumes it can walk the rows without scrolling).
        let peeked_range =
            if !(first_render || width_changed || height_forces_full_redraw || shrunk) {
                Self::compute_diff_range(&self.previous_lines, &lines)
            } else {
                None
            };
        let diff_above_viewport = peeked_range
            .map(|(first, _)| first < effective_viewport_top)
            .unwrap_or(false);
        let deletion_only_needs_full = peeked_range
            .map(|(first, _)| {
                if first < lines.len() {
                    return false;
                }
                // Pure-deletion case. Target row is the end of the new
                // (shorter) frame; if that's already above the visible
                // viewport, the cleanup move would land in scrollback.
                let target_row = lines.len().saturating_sub(1);
                let extra_lines = self.previous_lines.len().saturating_sub(lines.len());
                let term_height = usize::from(self.terminal.rows());
                target_row < effective_viewport_top
                    || (term_height > 0 && extra_lines > term_height)
            })
            .unwrap_or(false);

        let strategy: &'static str;
        let mut diff_range: Option<(usize, usize)> = None;
        // `pending_full_clear` distinguishes a post-`force_full_render`
        // render from a genuine first render. Both set `previous_lines`
        // empty, but only the former wants the clear sequence; see the
        // field doc for the rationale.
        let recover_full_clear = self.pending_full_clear;
        if first_render
            || width_changed
            || height_forces_full_redraw
            || shrunk
            || diff_above_viewport
            || deletion_only_needs_full
            || recover_full_clear
        {
            strategy = if recover_full_clear {
                "full(recover)"
            } else if first_render {
                "full(first_render)"
            } else if width_changed {
                "full(width_changed)"
            } else if height_forces_full_redraw {
                "full(height_changed)"
            } else if shrunk {
                "full(shrunk)"
            } else if deletion_only_needs_full {
                "full(deletion_only_unreachable)"
            } else {
                "full(diff_above_viewport)"
            };
            // Clear-on-paint is required for every full-render strategy
            // *except* a genuine first render on a freshly-started Tui,
            // which must preserve pre-existing shell output above. A
            // `recover_full_clear` path forces the clear even though
            // `first_render` is true, because the caller has explicitly
            // asked for a wipe.
            let clear = !first_render || recover_full_clear;
            self.full_render(&lines, clear);
            self.full_redraws += 1;
            self.pending_full_clear = false;
        } else {
            // Sync the recomputed viewport top into `self` before
            // `differential_render` reads it. Its end-of-run update
            // takes `max(self.previous_viewport_top, cursor_row -
            // height + 1)`, so starting from the stale value would
            // bias the tracker upward when the height shrinks.
            self.previous_viewport_top = effective_viewport_top;
            diff_range = self.differential_render(&lines);
            strategy = if diff_range.is_some() {
                "diff"
            } else {
                "diff(no-change)"
            };
        }

        // Phase 6: Position hardware cursor.
        if let Some(pos) = &cursor_pos {
            self.position_hardware_cursor(pos.row, pos.col, &lines);
        } else if self.hardware_cursor_currently_shown {
            // Only emit `\x1b[?25l` when transitioning from shown to
            // hidden. Re-emitting hide on every frame while already
            // hidden is a no-op at the protocol level, but some
            // terminals react to the sequence with a brief cursor
            // repaint — perceptible as flicker when the popup stays
            // open across many keystrokes.
            self.terminal.hide_cursor();
            self.hardware_cursor_currently_shown = false;
        }

        // Update state.
        self.previous_lines = lines;
        self.previous_width = current_width;
        self.previous_height = current_height;
        self.last_render_time = Some(StdInstant::now());
        self.render_requested = false;

        // Flush the debug record (if enabled) after all state is
        // settled so the logged `cursor_at_after`/`max_lines_rendered_after`
        // reflect the committed values for this frame.
        if let Some(path) = debug_log_path {
            let record = RenderDebugRecord {
                strategy,
                cursor_at_before,
                cursor_at_after: self.hardware_cursor_row,
                first_changed: diff_range.map(|(f, _)| f),
                last_changed: diff_range.map(|(_, l)| l),
                prev_len: prev_lines_snapshot.len(),
                new_len: self.previous_lines.len(),
                max_lines_rendered_before,
                max_lines_rendered_after: self.max_lines_rendered,
                width: current_width,
                height: current_height,
                width_changed,
                height_changed,
                cursor_pos: cursor_pos.map(|p| (p.row, p.col)),
                prev_lines_snapshot,
                new_lines_snapshot,
            };
            record.append_to(&path);
        }
    }

    /// Check if enough time has elapsed since the last render for a new one.
    pub fn should_render(&self) -> bool {
        match self.last_render_time {
            None => true,
            Some(t) => t.elapsed() >= MIN_RENDER_INTERVAL,
        }
    }

    /// Mark that a render is needed. Sets the `render_requested` flag and
    /// nudges the async event loop so a subsequent [`Tui::next_event`]
    /// call yields [`TuiEvent::Render`] once the throttle window elapses.
    ///
    /// Safe to call from synchronous code paths that also drive [`render`]
    /// directly: the flag is just a hint, and extra notifications on the
    /// async channel are harmless (coalesced by the throttle).
    pub fn request_render(&mut self) {
        self.render_requested = true;
        let _ = self.render_tx.send(());
    }

    /// Mark that a full, non-differential render is needed on the next
    /// pass. Clears the engine's diff state so every line is re-emitted
    /// even if the rendered content is byte-identical to the previous
    /// frame, and sets the render-requested flag so event loops that key
    /// off [`Tui::is_render_requested`] pick the frame up.
    ///
    /// The intended use is after an out-of-band terminal change — a clear
    /// screen, a resize the engine hasn't seen yet, a scrollback hijack —
    /// that would otherwise leave the diff state out of sync with what's
    /// actually on screen. If you only need to drop the cached diff state
    /// without also flagging a pending render (for example, because
    /// you're about to render directly in the same tick),
    /// [`Tui::force_full_render`] is the lower-level variant.
    pub fn request_full_render(&mut self) {
        self.force_full_render();
        self.render_requested = true;
        let _ = self.render_tx.send(());
    }

    /// Returns true if a render has been requested.
    pub fn is_render_requested(&self) -> bool {
        self.render_requested
    }

    /// Total number of full (non-differential) redraws the engine has performed
    /// since creation. Useful for tests that need to assert the engine took a
    /// particular rendering strategy (e.g. that a resize or shrink triggered a
    /// full redraw rather than a diff).
    pub fn full_redraws(&self) -> u64 {
        self.full_redraws
    }

    /// Total number of [`Tui::render`] calls that reached the paint
    /// phase (non-zero terminal dimensions), regardless of strategy.
    /// Includes full redraws, differential renders, and no-change
    /// renders alike.
    ///
    /// Primary use is in async coalescing tests: blast N render
    /// requests inside one throttle window, advance the clock past
    /// the window, and assert `total_renders()` incremented by
    /// exactly 1. Without the counter, tests had to rely on
    /// throttle-timing asserts that were prone to flaking on slow
    /// CI machines.
    pub fn total_renders(&self) -> u64 {
        self.total_renders
    }

    /// The historical high-water mark of the number of lines that have been
    /// rendered, used to drive clear-on-shrink decisions. Grows across
    /// renders and only resets when a full-clear render runs, so a
    /// transient component that briefly grew the working area keeps
    /// the engine honest about needing to clean up if content later
    /// dips below that peak.
    ///
    /// Exposed primarily for regression tests that want to verify the
    /// engine's internal bookkeeping matches the rendering decisions
    /// it's making.
    pub fn max_lines_rendered(&self) -> usize {
        self.max_lines_rendered
    }

    /// Enable or disable forcing a full redraw when rendered content shrinks.
    ///
    /// When enabled, a render whose line count is smaller than the engine's
    /// high-water mark takes the full-render path, which clears the screen
    /// region below the new content. When disabled (the default), the
    /// differential path handles shrink by clearing the trailing rows one
    /// at a time, which avoids visible flicker — particularly noticeable
    /// when a popup (e.g. the editor's `@`-fuzzy-file list) grows and
    /// shrinks rapidly as the user types. Enable this when you want a
    /// stronger guarantee that stale rows below the working area are
    /// wiped, accepting the cost of a periodic full repaint.
    pub fn set_clear_on_shrink(&mut self, enabled: bool) {
        self.clear_on_shrink = enabled;
    }

    /// Returns whether clear-on-shrink is enabled.
    pub fn clear_on_shrink(&self) -> bool {
        self.clear_on_shrink
    }

    /// Enable or disable the phase-4.5 line-width sanity check.
    ///
    /// When enabled (the default) the engine panics if any rendered
    /// line would exceed the current terminal width — silently letting
    /// an oversize line through corrupts the diff engine's cursor
    /// tracking on the very next frame, so failing loudly with a full
    /// frame dumped to `~/.aj/aj-tui-crash.log` gives callers a
    /// fighting chance to pinpoint the offending component.
    ///
    /// Disable this only for tests that intentionally probe
    /// compositor edge cases with inputs they know overflow (e.g.
    /// overlay positioning tests whose assertion is "didn't panic
    /// during composition", independent of whether the final frame
    /// is valid). Application code should leave it on.
    pub fn set_strict_line_widths(&mut self, enabled: bool) {
        self.strict_line_widths = enabled;
    }

    /// Returns whether the strict line-width check is enabled.
    pub fn strict_line_widths(&self) -> bool {
        self.strict_line_widths
    }

    /// Select how `full_render(clear=true)` wipes the screen before
    /// repainting. See [`FullClearMode`] for the semantics and
    /// tradeoffs of each mode.
    ///
    /// The default is [`FullClearMode::WholeScreen`] — the
    /// scroll-drift-resistant option that's correct for chat-style
    /// agents whose scrollback is the app. Switch to
    /// [`FullClearMode::BelowCursor`] when the TUI is a transient
    /// overlay on top of shell output that should be preserved above
    /// it.
    pub fn set_full_clear_mode(&mut self, mode: FullClearMode) {
        self.full_clear_mode = mode;
    }

    /// Returns the current full-clear mode.
    pub fn full_clear_mode(&self) -> FullClearMode {
        self.full_clear_mode
    }

    /// Whether hardware-cursor display is *enabled* (user preference).
    ///
    /// When `true` (the default), focus-aware components that embed
    /// `CURSOR_MARKER` in their rendered output cause the engine to
    /// show the real terminal cursor at the marker position — useful
    /// for IME candidate-window placement and for apps that want a
    /// visible caret in their text inputs.
    ///
    /// Set to `false` to globally suppress the hardware cursor even
    /// when markers are present. The inline marker that components
    /// embed is still stripped from the rendered output, so the only
    /// visible cursor indication becomes whatever the component
    /// itself paints (e.g. a reverse-video block rendered in the
    /// component's `render` method).
    ///
    /// Distinct from [`Self::hardware_cursor_currently_shown`], which
    /// is the engine's private tracking of *whether* the most recent
    /// escape sequence left the cursor visible.
    pub fn hardware_cursor_enabled(&self) -> bool {
        self.hardware_cursor_enabled
    }

    /// Set the user preference for showing the hardware cursor.
    /// See [`Self::hardware_cursor_enabled`] for semantics.
    ///
    /// Takes effect on the next render: if the flag is flipped off
    /// mid-flight and the cursor was currently shown, the next render
    /// emits `\x1b[?25l` to hide it (either via
    /// [`Self::position_hardware_cursor`] when a marker is still
    /// present, or via the no-marker branch in `render` otherwise).
    pub fn set_hardware_cursor_enabled(&mut self, enabled: bool) {
        self.hardware_cursor_enabled = enabled;
    }

    /// Whether the hardware cursor is currently shown, as last left
    /// by an emitted `\x1b[?25h` / `\x1b[?25l`. Engine-internal
    /// tracking; exposed primarily for regression tests and for
    /// apps that temporarily commandeer the terminal (animations,
    /// sleeps, shutdown confirmation) and want to know the state
    /// to restore afterward.
    ///
    /// Distinct from [`Self::hardware_cursor_enabled`], which is
    /// the user preference.
    pub fn hardware_cursor_currently_shown(&self) -> bool {
        self.hardware_cursor_currently_shown
    }

    /// Low-level paired setter for
    /// [`Self::hardware_cursor_currently_shown`]. Writes the state
    /// flag without emitting any escape sequence; this is almost
    /// never the right thing to call from application code. Most
    /// callers want [`Self::set_hardware_cursor_enabled`] (user
    /// preference) instead.
    pub fn set_hardware_cursor_currently_shown(&mut self, shown: bool) {
        self.hardware_cursor_currently_shown = shown;
    }

    /// Deprecated alias for [`Self::hardware_cursor_currently_shown`].
    /// Preserved so callers that reached into the state tracker don't
    /// break on the preference-vs-state split; new code should pick
    /// the accessor that matches its intent (`hardware_cursor_enabled`
    /// for the user preference, `hardware_cursor_currently_shown` for
    /// the state).
    #[deprecated(note = "split into hardware_cursor_enabled (preference) and \
                hardware_cursor_currently_shown (state)")]
    pub fn show_hardware_cursor(&self) -> bool {
        self.hardware_cursor_currently_shown
    }

    /// Deprecated alias for [`Self::set_hardware_cursor_currently_shown`].
    /// See the deprecation note on [`Self::show_hardware_cursor`].
    #[deprecated(note = "split into set_hardware_cursor_enabled (preference) and \
                set_hardware_cursor_currently_shown (state)")]
    pub fn set_show_hardware_cursor(&mut self, shown: bool) {
        self.hardware_cursor_currently_shown = shown;
    }

    /// Dispatch an input event to the focused component or overlay.
    ///
    /// Before routing, the event passes through every listener registered
    /// via [`Tui::add_input_listener`] in insertion order. A listener can
    /// [`InputListenerAction::Consume`] the event (stopping dispatch
    /// entirely), [`InputListenerAction::Rewrite`] it (so subsequent
    /// listeners and the dispatch path see the new event), or
    /// [`InputListenerAction::Pass`] through unchanged.
    pub fn handle_input(&mut self, event: &InputEvent) {
        // Run pre-component listeners first. A `Consume` result short-
        // circuits every further step; a `Rewrite` result threads a new
        // event through the remaining listeners and the routing logic. If
        // no listener rewrites, we use the caller's original event.
        let mut rewritten: Option<InputEvent> = None;
        for slot in self.input_listeners.iter_mut() {
            let current = rewritten.as_ref().unwrap_or(event);
            match (slot.listener)(current) {
                InputListenerAction::Pass => {}
                InputListenerAction::Rewrite(new_event) => {
                    rewritten = Some(new_event);
                }
                InputListenerAction::Consume => return,
            }
        }
        let event_ref = rewritten.as_ref().unwrap_or(event);
        self.handle_input_after_listeners(event_ref);
    }

    /// Register a pre-component input listener. The listener runs on every
    /// [`Tui::handle_input`] call before any overlay / focus routing.
    ///
    /// Returns an [`InputListenerHandle`] that identifies the listener for
    /// later removal via [`Tui::remove_input_listener`]. Listeners are
    /// invoked in insertion order, so chaining two listeners where the first
    /// rewrites the event and the second observes the result works out of
    /// the box.
    pub fn add_input_listener<F>(&mut self, listener: F) -> InputListenerHandle
    where
        F: FnMut(&InputEvent) -> InputListenerAction + 'static,
    {
        let id = self.next_listener_id;
        self.next_listener_id += 1;
        self.input_listeners.push(InputListenerSlot {
            id,
            listener: Box::new(listener),
        });
        InputListenerHandle(id)
    }

    /// Remove a previously-registered input listener. Unknown handles are
    /// ignored (idempotent). The relative order of remaining listeners is
    /// preserved.
    pub fn remove_input_listener(&mut self, handle: InputListenerHandle) {
        self.input_listeners.retain(|slot| slot.id != handle.0);
    }

    /// Register a global debug hook, fired before input routing when
    /// the user presses `Shift+Ctrl+D`.
    ///
    /// The callback runs once per matching press and has exclusive
    /// access to `FnMut` state across calls. A previously-registered
    /// callback is replaced; passing the hook a no-op closure does
    /// *not* clear it, use [`Tui::clear_on_debug`] for that.
    ///
    /// When a callback is registered, the `Shift+Ctrl+D` event is not
    /// forwarded to overlays or focused components — it's treated as
    /// a reserved chord. Apps that want `Shift+Ctrl+D` to reach a
    /// component instead should not register a debug hook.
    pub fn set_on_debug<F>(&mut self, callback: F)
    where
        F: FnMut() + 'static,
    {
        self.on_debug = Some(Box::new(callback));
    }

    /// Remove any previously-registered debug hook. After this call,
    /// `Shift+Ctrl+D` flows through to normal input routing.
    pub fn clear_on_debug(&mut self) {
        self.on_debug = None;
    }

    /// Internal dispatch path after the input-listener chain has had its
    /// say. Split from [`Tui::handle_input`] so the listener loop has
    /// exclusive access to `self.input_listeners` without conflicting
    /// borrows of the rest of the engine state.
    fn handle_input_after_listeners(&mut self, event: &InputEvent) {
        // pi-tui's `TUI.handleInput` has a `consumeCellSizeResponse`
        // branch here that regex-matches `\x1b[6;<h>;<w>t` and feeds
        // `setCellDimensions` for image rendering. We don't, by design:
        // we have no image rendering to consume the dimensions, and
        // bytes are already parsed by the time they reach this
        // dispatcher — the structural drop happens at crossterm's
        // `Parser::advance` Err arm. See the rustdoc on `TryFrom<Event>`
        // in `keys.rs` and PORTING.md E5 for the trace.

        // Handle resize events.
        if let InputEvent::Resize(_, _) = event {
            self.request_render();
            return;
        }

        // Global debug chord. Fires before focus healing and input
        // routing so an app can grab a state snapshot even when an
        // overlay would otherwise have swallowed the event. The chord
        // is consumed: a registered callback means components never
        // see `Shift+Ctrl+D`.
        if self.on_debug.is_some() && key_id_matches(event, "shift+ctrl+d") {
            if let Some(callback) = self.on_debug.as_mut() {
                callback();
            }
            return;
        }

        let cols = self.terminal.columns();
        let rows = self.terminal.rows();

        // Focus heal: if the focused overlay has become invisible
        // (hidden flag set, or `visible` callback returning false for
        // the current dimensions), transfer focus to the topmost
        // visible *capturing* overlay. Non-capturing overlays are
        // deliberately skipped even if they sit above the next
        // capturing overlay in focus order; doing otherwise would let
        // a non-capturing backdrop silently steal input that was
        // intended for a modal below it.
        //
        // If no visible capturing overlay remains, the saved pre-focus
        // target (the root component focus at the time the overlay
        // first stole focus) is restored. That's the same shape as
        // `promote_focus_after_unfocus`, but inlined here because
        // `promote_focus_after_unfocus` also bumps focus_order on the
        // new target, which would perturb the visual z-order — we
        // only want to reroute input, not reorder the stack.
        if let Some(focused_id) = self.focused_overlay_id {
            let focused_visible = self
                .overlays
                .iter()
                .find(|e| e.id == focused_id)
                .map(|e| overlay_is_visible(e, cols, rows))
                .unwrap_or(false);
            if !focused_visible {
                // Find the next topmost visible capturing overlay.
                let fallback_id = self
                    .overlays
                    .iter()
                    .filter(|e| !e.options.non_capturing && overlay_is_visible(e, cols, rows))
                    .max_by_key(|e| e.focus_order)
                    .map(|e| e.id);

                // Clear focused state on the old (invisible) overlay.
                if let Some(entry) = self.overlays.iter_mut().find(|e| e.id == focused_id) {
                    entry.component.set_focused(false);
                }

                match fallback_id {
                    Some(id) => {
                        self.focused_overlay_id = Some(id);
                        if let Some(entry) = self.overlays.iter_mut().find(|e| e.id == id) {
                            entry.component.set_focused(true);
                            entry.routing_active = true;
                        }
                    }
                    None => {
                        self.focused_overlay_id = None;
                        if let Some(saved) = self.saved_focus.take() {
                            match saved {
                                FocusTarget::Child(idx) => {
                                    self.focused_component_index = Some(idx);
                                    if let Some(child) = self.root.get_mut(idx) {
                                        child.set_focused(true);
                                    }
                                }
                                FocusTarget::None => {
                                    self.focused_component_index = None;
                                }
                            }
                        }
                    }
                }
                // Fall through to the normal dispatch path below, which
                // will now see the healed focus state.
            }
        }

        // Explicit overlay focus wins over stack-order routing, but only
        // if the overlay is currently visible (not hidden and not gated
        // out by its `visible` callback). An invisible focused overlay
        // should not swallow input; let it fall through to routing.
        if let Some(id) = self.focused_overlay_id {
            let visible = self
                .overlays
                .iter()
                .find(|e| e.id == id)
                .map(|e| overlay_is_visible(e, cols, rows))
                .unwrap_or(false);
            if visible {
                if let Some(entry) = self.overlays.iter_mut().find(|e| e.id == id) {
                    if Self::should_dispatch_to(entry.component.as_ref(), event) {
                        entry.component.handle_input(event);
                        self.request_render();
                    }
                    return;
                }
            }
        }

        // Route to the topmost visible capturing overlay in the routing
        // pool. An overlay is a candidate only if it's `routing_active`
        // (set by auto-focus on show, cleared by explicit unfocus),
        // not hidden, and its `visible` callback (if any) passes.
        let overlay_idx = self
            .overlays
            .iter_mut()
            .enumerate()
            .filter(|(_, e)| e.routing_active && !e.options.non_capturing)
            .filter(|(_, e)| overlay_is_visible(e, cols, rows))
            .max_by_key(|(_, e)| e.focus_order)
            .map(|(i, _)| i);

        if let Some(idx) = overlay_idx {
            let entry = &mut self.overlays[idx];
            if Self::should_dispatch_to(entry.component.as_ref(), event) {
                entry.component.handle_input(event);
                self.request_render();
            }
            return;
        }

        // Route to focused root component.
        if let Some(idx) = self.focused_component_index {
            if let Some(child) = self.root.get_mut(idx) {
                if Self::should_dispatch_to(child.as_ref(), event) {
                    child.handle_input(event);
                    self.request_render();
                }
            }
        }
    }

    /// Whether `event` should be delivered to `component` given the
    /// component's [`Component::wants_key_release`] opt-in. Key-release
    /// events are dropped for components that don't opt in; everything
    /// else (press, repeat, paste) is delivered unchanged.
    fn should_dispatch_to(component: &dyn Component, event: &InputEvent) -> bool {
        !event.is_key_release() || component.wants_key_release()
    }

    /// Set which root child component has focus (by index). Calls
    /// [`Component::set_focused`] on the newly focused child (and on the
    /// previously focused one) so stateful components can react.
    pub fn set_focus(&mut self, index: Option<usize>) {
        // Clear the previous root focus state.
        if let Some(prev) = self.focused_component_index {
            if let Some(child) = self.root.get_mut(prev) {
                child.set_focused(false);
            }
        }
        // Clear any overlay focus too: setting root focus takes over.
        if let Some(id) = self.focused_overlay_id.take() {
            if let Some(entry) = self.overlays.iter_mut().find(|e| e.id == id) {
                entry.component.set_focused(false);
            }
        }
        self.saved_focus = None;
        self.focused_component_index = index;
        if let Some(idx) = index {
            if let Some(child) = self.root.get_mut(idx) {
                child.set_focused(true);
            }
        }
    }

    /// Transfer focus to an overlay, saving the current focus so
    /// [`Tui::unfocus_overlay`] can restore it.
    ///
    /// This is the mechanism non-capturing overlays use to opt in to
    /// receiving input: they do not capture focus automatically on
    /// [`Tui::show_overlay`], but `focus_overlay` promotes them ahead of
    /// the normal routing stack.
    ///
    /// Focusing an overlay also bumps its rendering z-order so it
    /// composites on top of any previously-shown overlays at the same
    /// position. This matches the intuition that focusing a window
    /// brings it forward.
    ///
    /// No-op when the overlay is hidden (via [`Tui::set_overlay_hidden`])
    /// or no longer present in the stack.
    pub fn focus_overlay(&mut self, handle: &OverlayHandle) {
        let id = handle.id();

        // Guard: only focusable if the overlay exists and is visible.
        let entry_is_focusable = self.overlays.iter().any(|e| e.id == id && !e.hidden);
        if !entry_is_focusable {
            return;
        }

        // Save the currently-focused target before overwriting. Only save
        // if we haven't already (so nested focus_overlay calls don't lose
        // the original).
        if self.saved_focus.is_none() {
            self.saved_focus = Some(match self.focused_component_index {
                Some(idx) => FocusTarget::Child(idx),
                None => FocusTarget::None,
            });
        }

        // Clear focus on whatever currently has it.
        if let Some(prev_idx) = self.focused_component_index.take() {
            if let Some(child) = self.root.get_mut(prev_idx) {
                child.set_focused(false);
            }
        }
        if let Some(prev_overlay_id) = self.focused_overlay_id.replace(id) {
            if prev_overlay_id != id {
                if let Some(entry) = self.overlays.iter_mut().find(|e| e.id == prev_overlay_id) {
                    entry.component.set_focused(false);
                }
            }
        }

        // Set focus on the new overlay, mark it active for routing, and
        // bump its z-order.
        let next_order = self.next_focus_order;
        self.next_focus_order += 1;
        if let Some(entry) = self.overlays.iter_mut().find(|e| e.id == id) {
            entry.component.set_focused(true);
            entry.focus_order = next_order;
            entry.routing_active = true;
        }
        self.request_render();
    }

    /// Restore the focus saved by a prior [`Tui::focus_overlay`] call.
    /// No-op if this overlay is not currently focused.
    ///
    /// Unlike [`Tui::hide_overlay`] and [`Tui::set_overlay_hidden`], this
    /// does not promote to the next-topmost capturing overlay: `unfocus`
    /// is the explicit "undo my focus request" operation and restores
    /// the pre-focus target directly. Use `hide_overlay` when the
    /// overlay is actually going away.
    ///
    /// Unfocus also removes the overlay from the stack-order routing
    /// pool: once a user explicitly asks for focus back, input should
    /// not silently route to the overlay just because it sits on top
    /// of the stack. Re-enter the pool by calling
    /// [`Tui::focus_overlay`] again.
    pub fn unfocus_overlay(&mut self, handle: &OverlayHandle) {
        if self.focused_overlay_id != Some(handle.id()) {
            return;
        }
        // Clear overlay focus and drop out of the routing pool.
        if let Some(entry) = self.overlays.iter_mut().find(|e| e.id == handle.id()) {
            entry.component.set_focused(false);
            entry.routing_active = false;
        }
        self.focused_overlay_id = None;

        // Restore the saved focus target, if any.
        if let Some(saved) = self.saved_focus.take() {
            match saved {
                FocusTarget::Child(idx) => {
                    self.focused_component_index = Some(idx);
                    if let Some(child) = self.root.get_mut(idx) {
                        child.set_focused(true);
                    }
                }
                FocusTarget::None => {
                    self.focused_component_index = None;
                }
            }
        }
        self.request_render();
    }

    /// Returns whether this overlay currently owns focus via
    /// [`Tui::focus_overlay`].
    pub fn is_overlay_focused(&self, handle: &OverlayHandle) -> bool {
        self.focused_overlay_id == Some(handle.id())
    }

    /// Toggle an overlay's visibility without removing it from the stack.
    /// A hidden overlay is skipped by compositing and input routing.
    ///
    /// Hiding a currently-focused overlay forces focus to be reassigned:
    /// to the next-topmost visible capturing overlay if one exists,
    /// otherwise back to the saved pre-focus target.
    ///
    /// Unhiding a capturing overlay bumps its rendering z-order to the
    /// top of the overlay stack and transfers focus to it. Non-
    /// capturing overlays are unhidden without any focus transfer or
    /// z-order change — they re-enter the stack at their original
    /// position and the previous focus holder keeps input. Callers
    /// that want a non-capturing overlay to receive input after
    /// unhiding must call [`Tui::focus_overlay`] explicitly.
    pub fn set_overlay_hidden(&mut self, handle: &OverlayHandle, hidden: bool) {
        // Guard against no-op transitions: a setHidden call that
        // doesn't change state is ignored so the focus_order bump on
        // unhide is only spent once per actual hide/show cycle.
        let id = handle.id();
        let previously_hidden = self.overlays.iter().find(|e| e.id == id).map(|e| e.hidden);
        let Some(was_hidden) = previously_hidden else {
            return;
        };
        if was_hidden == hidden {
            return;
        }

        if hidden {
            // Hide path: clear the `hidden` flag, then if this overlay
            // currently holds focus, promote.
            if let Some(entry) = self.overlays.iter_mut().find(|e| e.id == id) {
                entry.hidden = true;
                if self.focused_overlay_id == Some(id) {
                    entry.component.set_focused(false);
                    self.focused_overlay_id = None;
                    self.promote_focus_after_unfocus();
                }
            }
            self.request_render();
            return;
        }

        // Unhide path. A capturing overlay is promoted to the top of
        // the stack (new focus_order) and focus is transferred to it.
        // For non-capturing overlays, unhiding is pure — no z-order or
        // focus change.
        let is_non_capturing = self
            .overlays
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.options.non_capturing)
            .unwrap_or(true);
        if let Some(entry) = self.overlays.iter_mut().find(|e| e.id == id) {
            entry.hidden = false;
        }
        if !is_non_capturing {
            // `focus_overlay` bumps focus_order and transfers focus,
            // saving the prior target into `saved_focus` so
            // unfocus/hide can restore it later. Handle is cheap to
            // rebuild from the ID.
            let handle = OverlayHandle { id };
            self.focus_overlay(&handle);
        } else {
            self.request_render();
        }
    }

    /// Start the TUI: bring the underlying terminal into its interactive
    /// state (raw mode, bracketed paste, keyboard enhancement, cursor
    /// hide) and take ownership of its input stream. Idempotent — calling
    /// `start` twice is a no-op past the first successful call.
    ///
    /// This does not trigger a first render; that happens on the first
    /// `render()` call (either manually or in response to a
    /// [`TuiEvent::Render`] from [`Tui::next_event`]).
    pub fn start(&mut self) -> std::io::Result<()> {
        self.terminal.start()?;
        if self.input_stream.is_none() {
            self.input_stream = self.terminal.take_input_stream();
        }
        // Establish the hardware-cursor baseline: hide on start. A
        // render that extracts `CURSOR_MARKER` later flips the
        // cursor back on (if `hardware_cursor_enabled`); a render
        // without a marker keeps it hidden. Without this baseline, a
        // freshly-constructed `Tui` backed by a terminal whose cursor
        // happens to start visible would stay visible until the first
        // marker-less render explicitly toggled it off.
        self.terminal.hide_cursor();
        self.hardware_cursor_currently_shown = false;
        // Publish the initial terminal dimensions to every
        // [`RenderHandle`]. Without this, components that consult
        // [`RenderHandle::terminal_rows`] (the editor's auto-sized
        // scroll window, for one) before the first `render()` see the
        // construction-time `0` placeholder and fall back to their
        // own default. `render()` republishes on every frame so any
        // subsequent resize is observable.
        self.term_rows
            .store(self.terminal.rows(), Ordering::Relaxed);
        self.term_cols
            .store(self.terminal.columns(), Ordering::Relaxed);
        Ok(())
    }

    /// Await the next [`TuiEvent`].
    ///
    /// Input events from the terminal are forwarded immediately.
    /// Renders are coalesced: multiple requests inside one
    /// `render_interval` collapse into a single [`TuiEvent::Render`].
    ///
    /// The first call (on a freshly-started `Tui`) will produce a
    /// [`TuiEvent::Render`] without an explicit request unless
    /// [`Tui::set_initial_render`] was called with `false`. This makes
    /// the first frame appear without bootstrap ceremony.
    ///
    /// Returns `None` once the terminal's input stream has ended *and*
    /// no [`RenderHandle`] remains alive — at that point the event
    /// loop has no source of work left. In practice this only happens
    /// during shutdown.
    pub async fn next_event(&mut self) -> Option<TuiEvent> {
        loop {
            // Lazily initialize the throttle. `interval_at(now + step, step)`
            // skips the immediate first tick so `tick().await` only fires
            // after a request has had a chance to arrive.
            if self.throttle.is_none() {
                let mut t = interval_at(
                    TokioInstant::now() + self.render_interval,
                    self.render_interval,
                );
                t.set_missed_tick_behavior(MissedTickBehavior::Delay);
                self.throttle = Some(t);
            }

            // Seed the implicit initial render once.
            if self.initial_render {
                self.initial_render = false;
                self.render_requested = true;
            }

            // Disjoint borrows so each select branch can touch a
            // different field of `self` without aliasing the whole
            // struct mutably.
            let Self {
                input_stream,
                render_rx,
                throttle,
                render_requested,
                ..
            } = self;
            let throttle = throttle.as_mut().expect("throttle initialized above");

            tokio::select! {
                biased;

                maybe_input = async {
                    match input_stream.as_mut() {
                        Some(s) => s.next().await,
                        // If there's no input stream, block this branch
                        // forever so the other branches stay lively.
                        None => std::future::pending::<Option<InputEvent>>().await,
                    }
                } => {
                    match maybe_input {
                        Some(ev) => return Some(TuiEvent::Input(ev)),
                        None => {
                            // Input stream ended. Drop it so future
                            // iterations hit the `pending` branch.
                            *input_stream = None;
                            // Flush any pending render first.
                            if *render_requested {
                                throttle.tick().await;
                                *render_requested = false;
                                return Some(TuiEvent::Render);
                            }
                            // Otherwise keep looping; the render
                            // channel can still surface work.
                        }
                    }
                }

                maybe_signal = render_rx.recv() => {
                    match maybe_signal {
                        Some(()) => {
                            *render_requested = true;
                            // Fall through; the next select iteration
                            // will either coalesce more requests or
                            // let the throttle fire.
                        }
                        None => {
                            // No `RenderHandle` clones remain and the
                            // internal sender is gone. Flush any
                            // pending render, then fall back to input
                            // as the only source of work.
                            if *render_requested {
                                throttle.tick().await;
                                *render_requested = false;
                                return Some(TuiEvent::Render);
                            }
                            // If input_stream is also None, nothing
                            // can wake us; report shutdown.
                            if input_stream.is_none() {
                                return None;
                            }
                        }
                    }
                }

                _ = throttle.tick(), if *render_requested => {
                    *render_requested = false;
                    return Some(TuiEvent::Render);
                }
            }
        }
    }

    /// Move cursor below all content and restore the terminal for a clean
    /// exit.
    ///
    /// Calls [`Terminal::stop`] to release raw mode, bracketed paste, and
    /// any keyboard enhancement flags. Skipping this (or crashing before
    /// reaching it) leaves the user's shell in an unusable state. The
    /// corresponding panic hook installed by `ProcessTerminal` is a safety
    /// net for the crash case.
    ///
    /// The cursor is parked on a fresh line past the last rendered row so
    /// the returning shell prompt lands below the TUI's content rather
    /// than clobbering it. We use `\r\n` rather than `\x1b[nB` for the
    /// downward move for the same reason `differential_render` does:
    /// `CUD` clamps at the last visible row on the standard vt100
    /// region model, so a TUI whose bottom row sits on the terminal's
    /// last row (common when the shell prompt was at the bottom when
    /// `start` ran) would have the move silently squashed and the
    /// prompt would paint over the last rendered line.
    ///
    /// Idempotent: only the first call emits terminal-restore writes;
    /// subsequent calls (including the one from the `Drop` impl) are
    /// no-ops.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        // Move to one row past the last rendered content via scrolling
        // `\r\n`s. `lines_below` is the number of rows between the
        // current cursor and the last-content row; one extra `\r\n`
        // advances to the row after it, which is where we want the
        // shell prompt to resume.
        let lines_below = self
            .previous_lines
            .len()
            .saturating_sub(self.hardware_cursor_row + 1);
        let mut tail = String::with_capacity((lines_below + 1) * 2);
        for _ in 0..lines_below {
            tail.push_str("\r\n");
        }
        tail.push_str("\r\n");
        self.terminal.write(&tail);
        self.terminal.show_cursor();
        self.terminal.flush();
        self.terminal.stop();
        self.stopped = true;
    }

    /// Force a full re-render on the next render call (e.g. after clear screen).
    ///
    /// Clears the engine's diff state *and* sets
    /// [`Self::pending_full_clear`], so the next `render` takes the
    /// clear-before-paint branch instead of the first-render (no-clear)
    /// branch. That distinction matters: a caller who just asked for a
    /// forced repaint almost certainly wants the screen wiped of
    /// whatever the previous (possibly corrupted) frame left behind,
    /// whereas a genuine first render wants to preserve pre-existing
    /// shell output above the TUI.
    pub fn force_full_render(&mut self) {
        self.previous_lines.clear();
        self.previous_width = 0;
        self.previous_height = 0;
        self.hardware_cursor_row = 0;
        self.max_lines_rendered = 0;
        self.previous_viewport_top = 0;
        self.pending_full_clear = true;
    }

    // -- Private rendering methods --

    /// Panic cleanly if any rendered line would overflow the current
    /// terminal width. See the phase-4.5 block in [`Tui::render`] for
    /// the motivation. The panic hook installed by
    /// [`ProcessTerminal::start`] restores raw-mode/paste/keyboard
    /// state before the message surfaces, and a crash log with every
    /// rendered line (annotated with its visible width) is written to
    /// `~/.aj/aj-tui-crash.log` (override with `AJ_TUI_CRASH_LOG`) so
    /// the offending component is easy to pinpoint after the fact.
    fn validate_line_widths(&mut self, lines: &[String]) {
        if !self.strict_line_widths {
            return;
        }
        let width = usize::from(self.terminal.columns());
        if width == 0 {
            // The terminal reports no width (headless/test edge case).
            // Nothing to validate against.
            return;
        }
        for (i, line) in lines.iter().enumerate() {
            let w = visible_width(line);
            if w > width {
                let header = format!(
                    "aj-tui crash: rendered line {i} exceeds terminal width ({w} > {width}).\n\
                     Likely cause: a component returned a line whose visible width was\n\
                     larger than the width it was asked to render at, or an overlay\n\
                     composite under-accounted for a wide grapheme at a segment boundary.\n\
                     Emitting the frame would have corrupted the diff engine's cursor\n\
                     tracking on the next render, so this check fails the frame instead."
                );
                write_crash_log(&header, lines, width);
                // Stop before we panic so the terminal's raw-mode/paste
                // state is cleaned up deterministically. The panic hook
                // is a backstop in case some caller bypasses `stop`.
                self.terminal.stop();
                let log = resolve_crash_log_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<no crash-log path>".to_string());
                panic!(
                    "rendered line {i} exceeds terminal width ({w} > {width}); use \
                     `visible_width` to measure and truncate before returning from \
                     `Component::render`. Full frame written to: {log}"
                );
            }
        }
    }

    fn full_render(&mut self, lines: &[String], clear: bool) {
        let mut buf = String::new();
        buf.push_str(SYNC_BEGIN);

        if clear {
            match self.full_clear_mode {
                FullClearMode::WholeScreen => {
                    // `\x1b[2J`   erase entire viewport
                    // `\x1b[H`    cursor to absolute `(1, 1)` (home)
                    // `\x1b[3J`   erase scrollback (xterm extension,
                    //             ignored by terminals that don't
                    //             implement it — safe to always send)
                    //
                    // The `\x1b[H` is load-bearing: it resets the
                    // physical cursor to a known absolute position,
                    // which lets the subsequent repaint succeed even
                    // if `hardware_cursor_row` had drifted from
                    // physical reality (e.g. after a sequence of
                    // renders that scrolled the terminal via `\r\n`
                    // past its bottom row).
                    buf.push_str("\x1b[2J\x1b[H\x1b[3J");
                }
                FullClearMode::BelowCursor => {
                    // Move to the tracked top of the rendered area
                    // and erase below. Preserves pre-TUI shell
                    // output at the cost of assuming the tracked
                    // cursor row is correct.
                    if self.hardware_cursor_row > 0 {
                        buf.push_str(&format!("\x1b[{}A", self.hardware_cursor_row));
                    }
                    buf.push_str("\r");
                    buf.push_str("\x1b[J");
                }
            }
        }

        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                buf.push_str("\r\n");
            }
            buf.push_str(line);
        }

        buf.push_str(SYNC_END);
        self.terminal.write(&buf);
        self.terminal.flush();

        self.hardware_cursor_row = lines.len().saturating_sub(1);
        // When we emitted the clear-from-cursor escape the rendered area
        // has been wiped, so the high-water mark resets to the new
        // length. Otherwise (first render, or any future caller that
        // opts out of the clear) we can only grow the high-water mark,
        // because some prior render may have left rows on the terminal
        // that we haven't yet touched.
        if clear {
            self.max_lines_rendered = lines.len();
        } else {
            self.max_lines_rendered = self.max_lines_rendered.max(lines.len());
        }
        // Viewport-top update: in [`FullClearMode::WholeScreen`] we
        // just painted `lines.len()` rows starting at physical 0, so
        // the viewport shows the bottom-most `height` of them. In
        // [`FullClearMode::BelowCursor`] we painted starting at the
        // previous top-of-area, which could be anywhere; the exact
        // tracking there would need to know the shell offset, which
        // we don't. The `max(0, N - height)` formula is right for
        // WholeScreen and a defensible approximation for BelowCursor
        // (strictly, BelowCursor's viewport top is unchanged by a
        // same-height content that fits, and advances by the overflow
        // when it doesn't — which is what the formula captures).
        let height = usize::from(self.terminal.rows());
        self.previous_viewport_top = if height == 0 {
            0
        } else {
            lines.len().saturating_sub(height)
        };
    }

    /// Compute the smallest `[first..=last]` range of rows that differ
    /// between `prev` and `new`, or `None` if the two frames are
    /// byte-identical. Rows past the end of either side are treated
    /// as empty strings so shrinks and grows both surface as
    /// changes to the "missing" slot.
    fn compute_diff_range(prev: &[String], new: &[String]) -> Option<(usize, usize)> {
        let max_len = new.len().max(prev.len());
        let mut first_changed = None;
        let mut last_changed = None;
        for i in 0..max_len {
            let new_line = new.get(i).map(|s| s.as_str()).unwrap_or("");
            let old_line = prev.get(i).map(|s| s.as_str()).unwrap_or("");
            if new_line != old_line {
                if first_changed.is_none() {
                    first_changed = Some(i);
                }
                last_changed = Some(i);
            }
        }
        first_changed.zip(last_changed)
    }

    fn differential_render(&mut self, lines: &[String]) -> Option<(usize, usize)> {
        let (first, last) = Self::compute_diff_range(&self.previous_lines, lines)?;

        // Pure-deletion branch: every changed row is past the end of
        // the new frame. Walk a dedicated cleanup path that uses CUD
        // (clamps at the viewport bottom without scrolling) rather
        // than `\r\n` (scrolls on the last row). The render-strategy
        // selector has already filtered the unreachable sub-cases
        // (target above viewport, extra-lines > terminal height) so
        // we can assume the cleanup sequence fits inside the visible
        // area.
        if first >= lines.len() {
            self.differential_render_deletion_only(lines);
            return Some((first, last));
        }

        let prev = &self.previous_lines;
        let term_height = usize::from(self.terminal.rows());
        // `render_end` caps the main write loop at the last row of the
        // new frame. Rows beyond that (trailing rows being deleted)
        // are handled by a separate cleanup pass *after* the loop —
        // using CUD / `\x1b[2K` pairs rather than `\r\n`, so we don't
        // scroll content off-screen when the cleanup reaches the
        // viewport bottom. Letting the main loop's `\r\n`-between-rows
        // cascade into deleted-row territory would push existing rows
        // into scrollback as a side effect of clearing them.
        let render_end = last.min(lines.len().saturating_sub(1));

        let mut buf = String::new();
        buf.push_str(SYNC_BEGIN);

        // Move cursor to the first changed line, column 0. Going up
        // is a plain `\x1b[nA` (CUU stays within the visible region
        // and we never need it to leave it). Going down is N
        // `\r\n`s rather than `\x1b[nB`: CUD clamps at the last
        // visible row without scrolling, so a target row that lives
        // below the current viewport bottom (popup just opened at
        // the bottom of the terminal, component appended past the
        // last visible row, etc.) would be silently clipped and the
        // subsequent `\x1b[2K` + write would stomp whatever lived
        // on the last row. `\n` scrolls on the last row, which is
        // exactly the semantic we need: the terminal moves the
        // working area up, the cursor ends on a real new row, and
        // the viewport-top tracking below sees the `cursor_row`
        // advance past the previous viewport bottom and advances in
        // step.
        //
        // Upstream handles this with an explicit "if target >
        // viewport_bottom, emit enough `\r\n`s to scroll, then use
        // CUD within the now-shifted viewport" branch. Collapsing
        // both cases into unconditional `\r\n`-for-down is simpler
        // and produces equivalent output; the only cost is a
        // marginal byte count when the target already lives inside
        // the viewport (one `\r\n` is two bytes vs. `\x1b[nB`'s four
        // to six, so it's a net win once you account for the
        // leading `\r` the old path had to emit separately).
        let cursor_at = self.hardware_cursor_row;

        // `appendStart` shortcut: when the new frame strictly appends
        // rows (`first == prev.len() && first > 0`), move to row
        // `first - 1` (the last pre-existing row, always inside the
        // viewport) and emit `\r\n` instead of `\r` after the move.
        // The `\r\n` then lands us on `first`, possibly scrolling the
        // viewport if it was full — handled by the `\r\n`-for-down
        // semantics that already drive the main loop.
        //
        // Without the shortcut, the collapsed `\r\n`-for-down logic
        // would emit `\r\n` (to reach `first`) followed by a
        // redundant `\r` at the end of the move — one extra byte per
        // append frame.
        //
        // Both paths produce semantically equivalent output on every
        // terminal the engine targets; the shortcut is a byte-trim
        // for append-heavy workloads (streaming LLM output).
        let append_start = first == self.previous_lines.len()
            && first > 0
            && lines.len() > self.previous_lines.len();
        let move_target = if append_start { first - 1 } else { first };

        if move_target < cursor_at {
            buf.push_str(&format!("\x1b[{}A", cursor_at - move_target));
        } else if move_target > cursor_at {
            for _ in 0..(move_target - cursor_at) {
                buf.push_str("\r\n");
            }
        }
        if append_start {
            // The `\r\n` here advances from `first - 1` to `first`
            // (possibly scrolling); the cursor ends on `first` at
            // col 0 without a separate `\r`.
            buf.push_str("\r\n");
        } else {
            buf.push_str("\r");
        }

        // Walk the changed range, clearing and rewriting each row that
        // actually differs from the previous frame while skipping rows
        // whose contents are byte-identical. Skipping unchanged rows
        // inside the `[first..=render_end]` window is an important
        // reduction of per-frame bytes: with a component like the
        // editor whose popup sits a few rows below the input line, a
        // diff that spans editor → (unchanged border) → popup would
        // otherwise retransmit the static border (often an 80-column
        // `─` run, roughly a kilobyte of SGR-heavy bytes) on every
        // keystroke. That retransmission is the dominant source of
        // visible flicker on terminals where the BSU sync mode
        // (`2026`) is not honored end-to-end — notably tmux
        // configurations that strip the private mode before it
        // reaches the outer terminal.
        let mut cursor_row = first;
        for i in first..=render_end {
            if i > first {
                buf.push_str("\r\n");
                cursor_row = i;
            }
            let new_line = lines.get(i).map(String::as_str).unwrap_or("");
            let old_line = prev.get(i).map(String::as_str).unwrap_or("");
            if new_line == old_line {
                // Unchanged middle row: the `\r\n` above already
                // advanced the cursor past it, so there is nothing
                // more to emit. The very first row of the range
                // (`i == first`) is guaranteed to differ, so this
                // branch never trips on a row that would need its
                // `\x1b[2K`-clear skipped.
                continue;
            }
            buf.push_str("\x1b[2K");
            buf.push_str(new_line);
        }

        // Cleanup pass for trailing rows deleted by the shrink. We're
        // sitting at `cursor_row` = `render_end` = `lines.len() - 1`
        // (end of new content) after the main loop. Walk `extra_lines`
        // rows below it using CUD+`\r\x1b[2K` pairs — CUD clamps at the
        // viewport bottom, so if we reach it, the clears land on the
        // last row and no scroll is triggered. After clearing, move
        // back up to `render_end` with CUU so the tracked cursor row
        // matches the physical position.
        let extra_lines = prev.len().saturating_sub(lines.len());
        if extra_lines > 0 {
            // Step onto the first row past new content.
            buf.push_str("\x1b[1B");
            for i in 0..extra_lines {
                buf.push_str("\r\x1b[2K");
                if i + 1 < extra_lines {
                    buf.push_str("\x1b[1B");
                }
            }
            // Return to end of new content.
            buf.push_str(&format!("\x1b[{}A", extra_lines));
        }

        buf.push_str(SYNC_END);
        self.terminal.write(&buf);
        self.terminal.flush();

        self.hardware_cursor_row = cursor_row;
        self.max_lines_rendered = self.max_lines_rendered.max(lines.len());
        // Track viewport advance caused by the `\r\n`s we just
        // emitted. If the final cursor row sits below where the
        // previous frame's viewport ended, the terminal must have
        // scrolled that many rows during the diff render. The
        // `max(prev, ...)` ensures `previous_viewport_top` only moves
        // forward — a diff render whose cursor ends within the old
        // viewport shouldn't bump it backwards.
        if term_height > 0 {
            self.previous_viewport_top = self
                .previous_viewport_top
                .max(cursor_row.saturating_sub(term_height - 1));
        }
        Some((first, last))
    }

    /// Emit the cleanup sequence for a frame whose only changes are
    /// trailing-row deletions. Called by [`Self::differential_render`]
    /// when `first >= lines.len()`; the render-strategy selector has
    /// already routed unreachable cases (target row above the
    /// viewport, extra-line count exceeding the terminal height) to
    /// a full redraw.
    ///
    /// The move pattern is CUU/CUD + `\x1b[1B`/`\r\x1b[2K` pairs — all
    /// clamp-style cursor moves, none that can scroll. Uses `\r\n`
    /// nowhere on this path, so reaching the viewport bottom doesn't
    /// push any existing rows into scrollback.
    fn differential_render_deletion_only(&mut self, lines: &[String]) {
        let prev_len = self.previous_lines.len();
        let new_len = lines.len();
        debug_assert!(prev_len > new_len, "deletion-only path requires shrink");

        // `target_row` is the last row of the new frame (where we want
        // the cursor to land when we're done). Clamps to 0 for a
        // frame that shrunk to zero rows.
        let target_row = new_len.saturating_sub(1);
        let extra_lines = prev_len - new_len;
        let cursor_at = self.hardware_cursor_row;

        let mut buf = String::new();
        buf.push_str(SYNC_BEGIN);

        // Move the cursor onto `target_row` at column 0. Use CUD (not
        // `\r\n`) because we're deliberately avoiding any scroll on
        // this path — the whole point of the branch is to clean up
        // deleted rows *without* losing the rows that survived. The
        // strategy selector has guaranteed `target_row` is within the
        // current viewport, so CUD will land correctly.
        if target_row > cursor_at {
            buf.push_str(&format!("\x1b[{}B", target_row - cursor_at));
        } else if target_row < cursor_at {
            buf.push_str(&format!("\x1b[{}A", cursor_at - target_row));
        }
        buf.push_str("\r");

        // Clear each trailing row: step down one row with CUD, then
        // `\r\x1b[2K` to return to column 0 and wipe the line. The
        // `if i + 1 < extra_lines` guard skips the last CUD so the
        // subsequent CUU count matches the number of steps we took.
        buf.push_str("\x1b[1B");
        for i in 0..extra_lines {
            buf.push_str("\r\x1b[2K");
            if i + 1 < extra_lines {
                buf.push_str("\x1b[1B");
            }
        }
        // Return to end of new content.
        buf.push_str(&format!("\x1b[{}A", extra_lines));

        buf.push_str(SYNC_END);
        self.terminal.write(&buf);
        self.terminal.flush();

        self.hardware_cursor_row = target_row;
        // `max_lines_rendered` stays put: we haven't cleared the
        // screen, so any previously-higher high-water mark is still a
        // useful signal for clear-on-shrink decisions on later
        // frames. Viewport top also stays put — we didn't scroll.
    }

    fn position_hardware_cursor(&mut self, row: usize, col: usize, lines: &[String]) {
        // Defensive clamp on row. The caller (typically `render` after
        // `extract_cursor_position` returned a position) has already
        // walked `lines` to find the marker, so `row < lines.len()` is
        // the common case. The clamp covers the path where a future
        // caller passes an arbitrary row — without it, a request to
        // park the cursor past the end of content would emit `\r\n`s
        // that scroll the viewport and leave the tracker pointing at a
        // row that doesn't correspond to any rendered line.
        //
        // Clamping to `lines.len() - 1` (and 0 when `lines` is empty)
        // keeps the cursor on a row that has known content. Cols are
        // already `usize` so a negative col is impossible at the type
        // level.
        let row = if lines.is_empty() {
            0
        } else {
            row.min(lines.len() - 1)
        };

        let current = self.hardware_cursor_row;
        let mut buf = String::new();

        // See the matching comment in `differential_render`: `\x1b[nB`
        // clamps at the last visible row, so when a prior render has
        // scrolled the working area and the cursor sits at the
        // terminal's last row, a down-move that would logically land
        // on a row below the visible bottom actually stays put. Use
        // `\r\n` (which scrolls on the last row) to preserve the
        // tracked logical position.
        if row < current {
            buf.push_str(&format!("\x1b[{}A", current - row));
        } else if row > current {
            for _ in 0..(row - current) {
                buf.push_str("\r\n");
            }
        }
        // Move to absolute column (1-indexed).
        buf.push_str(&format!("\x1b[{}G", col + 1));

        // Honor the user preference. With the cursor *enabled*, emit
        // `\x1b[?25h` on the first frame that has a marker (and never
        // again until the cursor is hidden out from under us).
        // Disabled means "keep it invisible regardless of marker
        // presence" — still move the cursor to the right place
        // (some IMEs anchor their candidate window on the reported
        // cursor position even when the cursor is hidden), but emit
        // `\x1b[?25l` if we happen to currently be showing it.
        if self.hardware_cursor_enabled {
            if !self.hardware_cursor_currently_shown {
                self.terminal.show_cursor();
                self.hardware_cursor_currently_shown = true;
            }
        } else if self.hardware_cursor_currently_shown {
            self.terminal.hide_cursor();
            self.hardware_cursor_currently_shown = false;
        }

        self.terminal.write(&buf);
        self.terminal.flush();
        self.hardware_cursor_row = row;
    }

    fn composite_overlays(&mut self, lines: &mut Vec<String>, width: usize, height: usize) {
        // Short-circuit when the overlay stack is empty. Beyond the obvious
        // performance win, this is load-bearing for the strategy-selector:
        // the second pass below pads the line buffer up to terminal height
        // so overlay rows can be interpreted as viewport-relative. If that
        // padding ran on frames with no overlays, the post-composite
        // `lines.len()` would always be at least `terminal.rows()`, which
        // would break the `lines.len() < max_lines_rendered` shrink check
        // (a 6-row → 2-row shrink on a 10-row terminal would look like 10 →
        // 10 to the shrink detector). Keep this early-return.
        if self.overlays.is_empty() {
            return;
        }

        // Sort overlays by focus order (lowest first = painted first).
        // `width`/`height` are `usize` here but `overlay_is_visible`
        // wants `u16` to match the predicate signature. Both come from
        // `terminal.columns()` / `terminal.rows()` (which return `u16`)
        // so the conversion can never truncate; clamp to `u16::MAX`
        // defensively if a future code path widens beyond `u16` range.
        let cols = u16::try_from(width).unwrap_or(u16::MAX);
        let rows = u16::try_from(height).unwrap_or(u16::MAX);
        let mut visible_overlays: Vec<(usize, u64)> = self
            .overlays
            .iter()
            .enumerate()
            .filter(|(_, e)| overlay_is_visible(e, cols, rows))
            .map(|(i, e)| (i, e.focus_order))
            .collect();
        visible_overlays.sort_by_key(|(_, order)| *order);
        if visible_overlays.is_empty() {
            return;
        }

        // First pass: render each overlay and resolve its layout, tracking
        // the minimum working-buffer height the composition requires.
        struct RenderedOverlay {
            lines: Vec<String>,
            row: usize,
            col: usize,
            width: usize,
        }
        let mut rendered: Vec<RenderedOverlay> = Vec::with_capacity(visible_overlays.len());
        let mut min_lines_needed = lines.len();

        for (idx, _) in visible_overlays {
            let entry = &mut self.overlays[idx];
            // Width and max-height don't depend on overlay height, so we
            // resolve the layout with a placeholder height=0 first.
            let layout = resolve_overlay_layout(&entry.options, 0, width, height);
            let overlay_lines = entry.component.render(layout.width);
            // Apply max-height clamp (if any).
            let max_h = layout.max_height.unwrap_or(overlay_lines.len());
            let overlay_lines: Vec<String> = overlay_lines.into_iter().take(max_h).collect();
            // Re-resolve with the actual content height for final row / col.
            let layout = resolve_overlay_layout(&entry.options, overlay_lines.len(), width, height);
            min_lines_needed = min_lines_needed.max(layout.row + overlay_lines.len());
            rendered.push(RenderedOverlay {
                lines: overlay_lines,
                row: layout.row,
                col: layout.col,
                width: layout.width,
            });
        }

        // Second pass: pad the line buffer to the working height and
        // composite each overlay at viewport-relative coordinates.
        //
        // `working_height` ensures the buffer is at least terminal-tall
        // so overlay rows (which are expressed relative to the visible
        // viewport, not to absolute logical row 0) land where the
        // overlay's anchor says they should. Without the terminal-
        // height floor, a frame whose base content is shorter than the
        // terminal would composite a `TopLeft`-anchored overlay at
        // logical row 0 — but logical row 0 is not the top of the
        // visible viewport once any scroll has happened, so the
        // overlay would appear at the wrong position or not at all.
        //
        // `viewport_start = max(0, working_height - term_height)` is
        // the logical row at which the bottom-aligned viewport begins.
        // When base content fits in the terminal, `viewport_start` is
        // 0 and overlay rows are identical to logical rows. When base
        // content exceeds the terminal height, `viewport_start`
        // advances and every overlay row gets offset by the same
        // amount so the overlay tracks the visible viewport rather
        // than drifting off-screen.
        //
        // Deliberately excludes `max_lines_rendered` from the
        // `working_height` computation: consulting the historical
        // high-water mark here would make the working buffer grow
        // whenever the overlay stack had ever been tall, with no
        // shrink path, and the inflation feeds itself into pushing
        // content into scrollback on terminal widen.
        let working_height = lines.len().max(height).max(min_lines_needed);
        while lines.len() < working_height {
            lines.push(String::new());
        }
        let viewport_start = working_height.saturating_sub(height);

        for overlay in rendered {
            for (i, overlay_line) in overlay.lines.iter().enumerate() {
                let target_row = viewport_start + overlay.row + i;
                if target_row < lines.len() {
                    lines[target_row] = composite_line_at(
                        &lines[target_row],
                        overlay_line,
                        overlay.col,
                        overlay.width,
                        width,
                    );
                }
            }
        }
    }
}

/// Safety net: if a `Tui` is dropped without the application calling
/// [`Tui::stop`] explicitly (e.g. a `?`-early-return from the event
/// loop or an unwinding panic), the `Drop` impl still restores the
/// terminal. The explicit [`Tui::stop`] remains the recommended path
/// since it runs synchronously in the normal exit flow; this is
/// strictly a fallback. Because [`Tui::stop`] is idempotent, calling
/// it explicitly and then dropping is safe and does not double-write.
impl Drop for Tui {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cursor_marker_extraction() {
        let mut lines = vec![
            "first line".to_string(),
            format!("cursor{}here", CURSOR_MARKER),
        ];
        let pos = extract_cursor_position(&mut lines, 10);
        assert!(pos.is_some());
        let pos = pos.unwrap();
        assert_eq!(pos.row, 1);
        assert_eq!(pos.col, 6); // "cursor" is 6 chars wide
        // Marker should be stripped.
        assert!(!lines[1].contains(CURSOR_MARKER));
    }

    /// Regression for E7: a line with two markers should keep the
    /// second one after extraction. The original framework strips
    /// exactly the marker at `markerIndex`; we splice the same way so
    /// a buggy component that emits two markers on one row gets the
    /// stray exposed in the frame instead of silently scrubbed.
    #[test]
    fn extract_cursor_position_strips_only_the_first_marker_on_a_line() {
        let mut lines = vec![format!("left{m}middle{m}right", m = CURSOR_MARKER)];
        let pos = extract_cursor_position(&mut lines, 10);
        assert!(pos.is_some());
        let pos = pos.unwrap();
        assert_eq!(pos.row, 0);
        assert_eq!(pos.col, 4); // "left" is 4 chars wide
        // The first marker is gone, the second survives verbatim.
        assert_eq!(lines[0], format!("leftmiddle{}right", CURSOR_MARKER));
        assert_eq!(lines[0].matches(CURSOR_MARKER).count(), 1);
    }
}
