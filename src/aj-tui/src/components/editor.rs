//! Multi-line text editor component.
//!
//! Features:
//! - Word-wrapped rendering with scroll
//! - Grapheme-aware cursor movement
//! - Emacs keybindings (Ctrl+A/E/K/U/W/Y, Alt+B/F/D, etc.)
//! - Kill ring with accumulation
//! - Undo stack with coalescing
//! - Bracketed paste handling
//! - Cursor marker emission for IME support

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyModifiers};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use unicode_segmentation::UnicodeSegmentation;

use crate::ansi::{
    is_punctuation_grapheme, is_whitespace_grapheme, truncate_to_width, visible_width,
    wrap_text_with_ansi,
};
use crate::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSession, AutocompleteSuggestions,
    SessionInvalid, SuggestOpts,
};
use crate::component::{CURSOR_MARKER, Component};
use crate::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use crate::keybindings;
use crate::keys::{InputEvent, is_newline_event, key_id_matches};
use crate::kill_ring::KillRing;
use crate::tui::RenderHandle;
use crate::undo_stack::UndoStack;
use crate::word_wrap::{TextSegment, word_wrap_line_with_segments};

/// Debounce applied to `@`-attachment autocomplete requests to coalesce
/// rapid keystrokes into a single walk.
const ATTACHMENT_AUTOCOMPLETE_DEBOUNCE: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------------------
// Paste markers
// ---------------------------------------------------------------------------

/// Regex fragment: matches a single paste-marker token. Tokens look
/// like `[paste #1 +20 lines]` or `[paste #1 1234 chars]`. The
/// parenthesized capture group in position 1 is the numeric paste id.
///
/// Kept as a fragment rather than a compiled `Regex` so we can build it
/// into the larger document-scanning regex `paste_marker_regex` on
/// demand without taking a regex crate dep (we find markers with
/// manual scanning anyway, so a single `regex` import would be wasted).
const PASTE_MARKER_PREFIX: &str = "[paste #";

/// Find the next paste-marker token in `line` starting at byte offset
/// `from`, validating against `pastes`. Returns `(start, end, id)`
/// byte offsets of the matching span.
///
/// Tokens must be closed with `]` and the inner shape must match one
/// of the two known forms. A `[paste #99 +5 lines]` fragment with no
/// corresponding entry in `pastes` is skipped so that manually typed
/// marker-like text is not treated as atomic.
fn find_next_marker(
    line: &str,
    from: usize,
    pastes: &HashMap<u32, String>,
) -> Option<(usize, usize, u32)> {
    let bytes = line.as_bytes();
    let prefix = PASTE_MARKER_PREFIX.as_bytes();
    let mut i = from;
    while i + prefix.len() <= bytes.len() {
        if &bytes[i..i + prefix.len()] != prefix {
            i += 1;
            continue;
        }
        let after_prefix = i + prefix.len();
        // Parse the numeric id.
        let id_start = after_prefix;
        let mut id_end = id_start;
        while id_end < bytes.len() && bytes[id_end].is_ascii_digit() {
            id_end += 1;
        }
        if id_end == id_start {
            i += 1;
            continue;
        }
        let id: u32 = match line[id_start..id_end].parse() {
            Ok(v) => v,
            Err(_) => {
                i += 1;
                continue;
            }
        };
        // The tail is either " +N lines]" or " N chars]" or "]" (no counter).
        let rest = &line[id_end..];
        let end = if let Some(end_rel) = parse_marker_tail(rest) {
            id_end + end_rel
        } else {
            i += 1;
            continue;
        };
        if !pastes.contains_key(&id) {
            // Not a valid marker — skip and keep scanning.
            i += 1;
            continue;
        }
        return Some((i, end, id));
    }
    None
}

/// Parse the tail of a paste marker starting after the id digits.
///
/// Accepts the three valid shapes and returns the byte length of the
/// tail including the closing `]`:
///
/// - `"]"` → `Some(1)`
/// - `" +123 lines]"` → `Some(13)`
/// - `" 1234 chars]"` → `Some(12)`
///
/// Returns `None` on any other shape.
fn parse_marker_tail(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    if bytes.first() == Some(&b']') {
        return Some(1);
    }
    if bytes.first() != Some(&b' ') {
        return None;
    }
    let is_lines = bytes.get(1) == Some(&b'+');
    let mut pos = if is_lines { 2 } else { 1 };
    let digits_start = pos;
    while pos < bytes.len() && bytes[pos].is_ascii_digit() {
        pos += 1;
    }
    if pos == digits_start {
        return None;
    }
    let suffix = if is_lines { " lines]" } else { " chars]" };
    if rest[pos..].starts_with(suffix) {
        Some(pos + suffix.len())
    } else {
        None
    }
}

/// Returns the byte range of the paste marker that contains `col` on
/// the given line, or `None` if `col` falls outside every marker.
/// "Contains" means `start <= col < end` — a cursor sitting at the
/// marker's start or end byte is *not* inside it.
#[allow(dead_code)] // reserved for rendering / visual-line layout work
fn marker_containing(
    line: &str,
    col: usize,
    pastes: &HashMap<u32, String>,
) -> Option<(usize, usize)> {
    let mut i = 0;
    while let Some((s, e, _id)) = find_next_marker(line, i, pastes) {
        if col > s && col < e {
            return Some((s, e));
        }
        if s >= col {
            return None;
        }
        i = e;
    }
    None
}

/// Returns `Some((start, end))` if a paste marker begins exactly at
/// `col` on the given line.
fn marker_starting_at(
    line: &str,
    col: usize,
    pastes: &HashMap<u32, String>,
) -> Option<(usize, usize)> {
    find_next_marker(line, col, pastes)
        .filter(|(s, _, _)| *s == col)
        .map(|(s, e, _)| (s, e))
}

/// Returns `Some((start, end))` if a paste marker ends exactly at
/// `col` on the given line.
fn marker_ending_at(
    line: &str,
    col: usize,
    pastes: &HashMap<u32, String>,
) -> Option<(usize, usize)> {
    let mut i = 0;
    while let Some((s, e, _id)) = find_next_marker(line, i, pastes) {
        if e == col {
            return Some((s, e));
        }
        if s >= col {
            return None;
        }
        i = e;
    }
    None
}

// ---------------------------------------------------------------------------
// Character-class helpers used by word segmentation
// ---------------------------------------------------------------------------

/// Set of characters that, when typed *inside* an existing slash or
/// `@` context, keep the autocomplete popup open. Matches the regex
/// `[A-Za-z0-9.\-_]` used in the original trigger-gating logic.
fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')
}

// ---------------------------------------------------------------------------
// Undo stack
// ---------------------------------------------------------------------------

/// Snapshot of editor state for undo.
#[derive(Debug, Clone)]
struct EditorSnapshot {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
}

// ---------------------------------------------------------------------------
// Visual-line map
// ---------------------------------------------------------------------------

/// A single visual (screen) line produced by word-wrapping one logical
/// document line at the current layout width.
///
/// Byte-offset based: `start_col` is the index into `lines[logical_line]`
/// where the visible span begins, and `length` is the byte length of
/// that span. Constructed by [`Editor::build_visual_line_map`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisualLine {
    logical_line: usize,
    start_col: usize,
    length: usize,
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

/// Theme for the editor component.
///
/// Mirrors pi-tui's `EditorTheme` interface
/// (`packages/tui/src/components/editor.ts`). Pi-tui ships no upstream
/// default theme — the agent layer builds one from its central palette
/// and passes it to [`Editor::new`]. We deliberately do not provide a
/// `Default` impl: the tui crate stays palette-agnostic, and tests
/// build themes via `tests/support/themes.rs` (mirroring pi's
/// `packages/tui/test/test-themes.ts`).
///
/// The closures use `Arc` (and the nested `SelectListTheme` is also
/// `Clone`-able via `Arc`), so the editor can hand the same theme
/// configuration off to its autocomplete popups by cloning.
#[derive(Clone)]
pub struct EditorTheme {
    /// Style for the top/bottom border lines.
    pub border_color: Arc<dyn Fn(&str) -> String>,
    /// Theme for embedded select lists (e.g. slash-command and attachment
    /// autocomplete popups).
    pub select_list: SelectListTheme,
}

/// A multi-line text editor with word wrapping, Emacs keybindings,
/// kill ring, and undo.
pub struct Editor {
    /// The document: one string per logical line.
    lines: Vec<String>,
    /// Cursor position: line index.
    cursor_line: usize,
    /// Cursor position: byte offset within the line.
    cursor_col: usize,

    // Rendering state.
    scroll_offset: usize,
    /// Maximum number of visual rows the editor reveals before
    /// scrolling. `Some(n)` is an explicit override set by
    /// [`Editor::set_max_visible_lines`]; `None` (the default) auto-
    /// sizes from the current terminal height as `max(5, floor(rows *
    /// 0.3))` via the editor's [`crate::tui::RenderHandle`]. The
    /// effective value is computed by [`Editor::max_visible_lines`].
    max_visible_lines: Option<usize>,
    padding_x: usize,
    focused: bool,
    theme: EditorTheme,

    // Editing state.
    kill_ring: KillRing,
    undo_stack: UndoStack<EditorSnapshot>,
    last_action: LastAction,
    /// Sticky column for vertical movement.
    preferred_visual_col: Option<usize>,
    /// Byte offset within the source logical line that the cursor would
    /// have occupied had it not been snapped to an atomic-segment start
    /// during the last vertical move. Consulted by the next vertical
    /// move so the sticky-column math reflects the user's intent rather
    /// than the post-snap column. Cleared on any horizontal move or
    /// edit (via [`Editor::reset_sticky_state`]).
    snapped_from_cursor_col: Option<usize>,
    /// Width the editor last rendered at, measured in visible columns
    /// after padding has been subtracted. Consulted by vertical cursor
    /// movement to rebuild the visual-line map. Wrapped in a [`Cell`]
    /// because [`Component::render`] is `&self`. Defaults to 80 so
    /// navigation before the first render doesn't divide by zero.
    layout_width: Cell<usize>,
    /// Active character-jump mode, set by `Ctrl+]` (forward) or
    /// `Ctrl+Alt+]` (backward) and cleared by a second press of the
    /// same key, Escape, or by consuming the next printable character.
    jump_mode: Option<JumpDirection>,

    // -- Autocomplete --
    //
    // The editor owns its autocomplete state machine rather than deferring
    // to an overlay component: a completion popup pops up *inside* the
    // editor's own render output, and keystrokes have to be routed
    // between the editor (typing, cursor) and the popup (navigation,
    // accept) inline. Splitting it out would push routing logic into
    // every caller.
    autocomplete_provider: Option<std::sync::Arc<dyn AutocompleteProvider>>,
    /// `None` = no popup. `Some(kind)` = popup visible.
    autocomplete_state: Option<AutocompleteMode>,
    /// The suggestion list component backing the popup.
    autocomplete_list: Option<SelectList>,
    /// Prefix the current suggestion set matches against. The selected
    /// item's `value` replaces exactly this many characters before the
    /// cursor when applied.
    autocomplete_prefix: String,
    /// Max items visible in the popup before scrolling.
    autocomplete_max_visible: usize,

    // -- Streaming autocomplete session --
    //
    // For providers that opt into the streaming API (see
    // [`AutocompleteProvider::try_start_session`]), we hold a live
    // session here and drive it directly on keystrokes rather than
    // spawning a one-shot tokio task per edit. The session owns its
    // own walker + nucleo worker pool and publishes match updates
    // into `autocomplete_list` through `pump_autocomplete_session`.
    //
    // Mutually exclusive with the one-shot pipeline below: the
    // editor prefers the session when one exists, and falls back to
    // `dispatch_autocomplete_request` only when `try_start_session`
    // returned `None` for the current context (e.g. slash commands,
    // direct path completion).
    autocomplete_session: Option<Box<dyn AutocompleteSession>>,

    // -- Autocomplete async pipeline --
    //
    // `update_autocomplete` doesn't compute suggestions inline: it
    // snapshots the current buffer, cancels any in-flight request,
    // and spawns a worker task that does the (potentially expensive)
    // filesystem walk on `spawn_blocking`. Results flow back through
    // `autocomplete_rx`, which is drained at the top of
    // [`Editor::render`] and [`Editor::handle_input`] so the popup
    // state reflects the latest delivery before the frame is read. The
    // request id + snapshot guard in [`AutocompleteDelivery`] catches
    // any late arrivals that have been invalidated by newer typing.
    /// Monotonically-increasing token bumped on every request start.
    /// Used by the editor to discard results from a superseded request.
    autocomplete_request_id: u64,
    /// Cancellation token for the currently-pending request. Cancelled
    /// before a new request is spawned and when the popup is dismissed.
    autocomplete_cancel: Option<CancellationToken>,
    /// Join handle for the currently-pending request task. Kept so
    /// tests can await completion deterministically; production code
    /// relies on the cancel token + result channel and never joins.
    autocomplete_task: Option<JoinHandle<()>>,
    /// Results channel: workers send a [`AutocompleteDelivery`] here
    /// when they finish, and `drain_autocomplete_results` consumes it
    /// before the next render.
    autocomplete_rx: mpsc::UnboundedReceiver<AutocompleteDelivery>,
    autocomplete_tx: mpsc::UnboundedSender<AutocompleteDelivery>,
    /// Render handle for the editor. Two roles:
    ///
    /// 1. The autocomplete worker task uses it to wake the driver's
    ///    event loop when results arrive. Without this, the driver
    ///    will only repaint on the next input event.
    /// 2. [`Editor::max_visible_lines`] reads
    ///    [`crate::tui::RenderHandle::terminal_rows`] from it to
    ///    auto-size the editor's scroll window when the user has
    ///    not set an explicit cap.
    ///
    /// Required at construction: callers that aren't attached to a
    /// `Tui` (tests, isolated component construction) pass
    /// [`crate::tui::RenderHandle::detached`].
    autocomplete_render_handle: RenderHandle,

    // History (for up/down arrow when at first/last line).
    history: Vec<String>,
    history_index: Option<usize>,

    // Paste buffering (reserved for future bracketed paste tracking).
    #[allow(dead_code)]
    paste_buffer: Option<String>,

    /// Map of paste id → the original content the marker stands in
    /// for. Populated by [`Editor::handle_paste`] when a paste crosses
    /// the large-paste threshold; consulted by cursor navigation and
    /// by [`Editor::get_expanded_text`].
    pastes: HashMap<u32, String>,
    /// Monotonically-increasing source for paste ids. Never reset
    /// except on reconstruction (`submit` / explicit clear) so ids
    /// stay unique within a session and typed-by-hand marker-like
    /// strings (e.g. `[paste #99 +5 lines]`) don't collide with a
    /// real paste.
    paste_counter: u32,

    // Submitted text (polled by the main loop as an alternative to callbacks).
    submitted_text: Option<String>,

    /// Called when the user submits (Enter without Shift).
    pub on_submit: Option<Box<dyn FnMut(&str)>>,
    /// Called when the text changes.
    pub on_change: Option<Box<dyn FnMut(&str)>>,
    /// If true, Enter inserts a newline instead of submitting.
    pub disable_submit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    None,
    Kill,
    Yank,
    TypeWord,
}

/// Direction of an in-flight character-jump (`Ctrl+]` / `Ctrl+Alt+]`).
///
/// Forward jumps land on the first occurrence strictly *after* the
/// current cursor; backward jumps land on the last occurrence strictly
/// *before* the current cursor. Both are case-sensitive and scan
/// across logical line boundaries when the current line has no match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JumpDirection {
    Forward,
    Backward,
}

/// Why an autocomplete popup is currently showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutocompleteMode {
    /// Triggered implicitly by typing `@` or `/`; closes automatically
    /// when the typed prefix stops matching any suggestion or the
    /// cursor leaves the trigger context.
    Regular,
    /// Triggered explicitly by Tab; stays open across typing so the
    /// user can narrow down a result set, and accepts the first item
    /// on a second Tab.
    Force,
}

/// A message sent by an autocomplete worker task back to the editor
/// once the underlying provider returns. Consumed by
/// [`Editor::drain_autocomplete_results`].
struct AutocompleteDelivery {
    /// Identifier of the request that produced this delivery. Compared
    /// against `Editor::autocomplete_request_id` before any state is
    /// mutated; stale deliveries are dropped silently.
    request_id: u64,
    /// Snapshot of the editor state captured when the request was
    /// dispatched. Compared against the current state before applying
    /// the suggestion list — if the user has kept typing, the snapshot
    /// will no longer match and the delivery is discarded. Belt-and-
    /// braces on top of the request-id check.
    snapshot: AutocompleteSnapshot,
    /// The suggestion list itself, or `None` if the provider returned
    /// no suggestions (or the request was cancelled mid-flight).
    suggestions: Option<AutocompleteSuggestions>,
    /// The popup mode the request was dispatched in. Preserved through
    /// the round-trip so late deliveries still open the popup in the
    /// right mode.
    mode: AutocompleteMode,
    /// Whether the worker should auto-apply a single result (the Tab /
    /// force path). `false` for implicit `@` and `/` triggers, which
    /// only ever open the popup.
    auto_apply_single: bool,
}

/// Captured editor state used to detect stale autocomplete deliveries.
/// Any field changing between dispatch and delivery means the user has
/// moved on; the result no longer applies to the current cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AutocompleteSnapshot {
    text: String,
    cursor_line: usize,
    cursor_col: usize,
}

/// What should happen after the autocomplete Enter handler runs, from
/// the perspective of the outer key dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnterOutcome {
    /// Completion was applied (or the popup dismissed) and no further
    /// action should be taken for this Enter keystroke.
    Consumed,
    /// Completion was applied and the outer dispatcher should continue
    /// on to its usual Enter-submits-the-message path. Produced only
    /// when the completed prefix was a slash-command *name*
    /// (`/clear`, `/delete`, etc.) so the user experience is
    /// `type /clear → autocomplete completes it → Enter submits` in
    /// a single keystroke.
    FallThroughToSubmit,
}

impl Editor {
    /// Maximum number of history entries retained by [`Editor::add_to_history`].
    /// Once reached, each new entry drops the oldest one.
    pub const HISTORY_LIMIT: usize = 100;

    /// Create a new empty editor.
    ///
    /// `handle` is the editor's render handle (used by the autocomplete
    /// pipeline to wake the driver and by [`Editor::max_visible_lines`]
    /// to read terminal dimensions). Callers attached to a `Tui` pass
    /// `tui.handle()`; standalone callers (tests, isolated
    /// construction) pass [`RenderHandle::detached`].
    ///
    /// Mirrors pi-tui's `new Editor(tui, theme, options?)` constructor,
    /// which takes the theme as a required argument. The tui crate
    /// stays palette-agnostic; build the theme from the agent's central
    /// palette (or a test fixture) and pass it in.
    pub fn new(handle: RenderHandle, theme: EditorTheme) -> Self {
        let (autocomplete_tx, autocomplete_rx) = mpsc::unbounded_channel();
        Self {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            scroll_offset: 0,
            max_visible_lines: None,
            padding_x: 0,
            focused: false,
            theme,
            kill_ring: KillRing::default(),
            undo_stack: UndoStack::default(),
            last_action: LastAction::None,
            preferred_visual_col: None,
            snapped_from_cursor_col: None,
            layout_width: Cell::new(80),
            jump_mode: None,
            autocomplete_provider: None,
            autocomplete_state: None,
            autocomplete_list: None,
            autocomplete_prefix: String::new(),
            autocomplete_max_visible: 5,
            autocomplete_session: None,
            autocomplete_request_id: 0,
            autocomplete_cancel: None,
            autocomplete_task: None,
            autocomplete_rx,
            autocomplete_tx,
            autocomplete_render_handle: handle,
            history: Vec::new(),
            history_index: None,
            paste_buffer: None,
            pastes: HashMap::new(),
            paste_counter: 0,
            submitted_text: None,
            on_submit: None,
            on_change: None,
            disable_submit: false,
        }
    }

    /// Get the full text content (lines joined with newlines).
    pub fn get_text(&self) -> String {
        self.lines.join("\n")
    }

    /// Current cursor position as `(line, col)`, where `col` counts Unicode
    /// scalar values (chars) from the start of the line, *not* bytes.
    ///
    /// Byte-offset internals are a private implementation detail; this
    /// accessor's unit is stable and portable across platforms. For
    /// pure-ASCII input `col` equals the byte offset.
    pub fn cursor(&self) -> (usize, usize) {
        let col = self.current_line()[..self.cursor_col].chars().count();
        (self.cursor_line, col)
    }

    /// A fresh, owned copy of the current document lines. Mutating the
    /// returned vector does not affect the editor.
    pub fn lines(&self) -> Vec<String> {
        self.lines.clone()
    }

    /// Take the submitted text, if any. This is set when the user presses
    /// Enter (without Shift) and `disable_submit` is false. Calling this
    /// clears the submitted state, so it returns `Some` at most once per
    /// submit event. Use this as an alternative to the `on_submit` callback.
    pub fn take_submitted(&mut self) -> Option<String> {
        self.submitted_text.take()
    }

    /// Set the text content, replacing everything. Cursor moves to end.
    pub fn set_text(&mut self, text: &str) {
        self.save_undo();
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(|s| s.to_string()).collect()
        };
        self.cursor_line = self.lines.len() - 1;
        self.cursor_col = self.lines[self.cursor_line].len();
        self.reset_sticky_state();
        // Setting text wholesale is a hard break from whatever interactive
        // chain was going on — kill accumulation, yank-pop cycles, or
        // word-repeat coalescing all must stop here. Otherwise a sequence
        // like `set_text("a"); ctrl_w; set_text("b"); ctrl_w;` would
        // accumulate "ab" into a single kill-ring entry instead of two.
        self.last_action = LastAction::None;
        // Exit history browsing mode. If `history_up` had pushed a
        // draft entry to the end of the ring when browsing started, drop
        // it — `set_text` is replacing the draft with the caller's content.
        if self.history_index.is_some() {
            self.history.pop();
            self.history_index = None;
        }
        self.fire_change();
    }

    /// Set an explicit cap on the number of visual rows the editor
    /// reveals before scrolling.
    ///
    /// Without this call, the editor auto-sizes the cap from the
    /// current terminal height via its [`crate::tui::RenderHandle`]:
    /// `max(5, floor(rows * 0.3))`. Calling this method opts the
    /// editor out of auto-sizing for the rest of its lifetime;
    /// callers that want to revert to auto-sizing should construct a
    /// fresh editor (or call [`Editor::clear_max_visible_lines`]).
    pub fn set_max_visible_lines(&mut self, max: usize) {
        self.max_visible_lines = Some(max);
    }

    /// Drop any explicit override set via
    /// [`Editor::set_max_visible_lines`] and revert to auto-sizing
    /// from the terminal height.
    pub fn clear_max_visible_lines(&mut self) {
        self.max_visible_lines = None;
    }

    /// Effective cap on visual rows revealed before scrolling.
    ///
    /// Resolution order:
    /// 1. An explicit value passed to [`Editor::set_max_visible_lines`].
    /// 2. `max(5, floor(terminal_rows * 0.3))`, where `terminal_rows`
    ///    comes from the editor's [`crate::tui::RenderHandle`].
    /// 3. `5` if no handle is wired or the handle reports `0` rows
    ///    (e.g. before [`crate::tui::Tui::start`] / first render).
    pub fn max_visible_lines(&self) -> usize {
        if let Some(explicit) = self.max_visible_lines {
            return explicit.max(1);
        }
        let rows = usize::from(self.autocomplete_render_handle.terminal_rows());
        // `max(5, floor(rows * 0.3))` mirrors the original framework's
        // formula. The `floor` falls naturally out of integer
        // multiply-then-divide.
        ((rows * 3) / 10).max(5)
    }

    /// Set horizontal padding.
    pub fn set_padding_x(&mut self, padding: usize) {
        self.padding_x = padding;
    }

    /// Current horizontal padding, in columns. Matches the value last
    /// passed to [`Self::set_padding_x`] (default `0`).
    pub fn padding_x(&self) -> usize {
        self.padding_x
    }

    /// Set the editor theme.
    pub fn set_theme(&mut self, theme: EditorTheme) {
        self.theme = theme;
    }

    /// Install an autocomplete provider. Replaces whatever was there;
    /// installing a new provider always cancels any active popup and
    /// any in-flight async request.
    ///
    /// Takes an [`std::sync::Arc`] because the editor hands a cloned
    /// reference to every spawned worker task. `Arc<dyn
    /// AutocompleteProvider>` is cheap to share across threads because
    /// the trait requires `Send + Sync`.
    pub fn set_autocomplete_provider(
        &mut self,
        provider: std::sync::Arc<dyn AutocompleteProvider>,
    ) {
        self.cancel_autocomplete();
        self.autocomplete_provider = Some(provider);
    }

    /// Whether the autocomplete popup is currently visible.
    pub fn is_showing_autocomplete(&self) -> bool {
        self.autocomplete_state.is_some()
    }

    /// Maximum number of suggestions rendered in the popup before the
    /// list starts scrolling. Clamped to `[3, 20]`.
    pub fn set_autocomplete_max_visible(&mut self, max: usize) {
        self.autocomplete_max_visible = max.clamp(3, 20);
    }

    /// Current autocomplete popup height cap, in rows. Reflects the
    /// clamp range `[3, 20]` applied by [`Self::set_autocomplete_max_visible`]
    /// (default `5`).
    pub fn autocomplete_max_visible(&self) -> usize {
        self.autocomplete_max_visible
    }

    /// Add a string to the history (for up/down arrow navigation).
    ///
    /// Ignores whitespace-only strings and refuses to append an entry that
    /// duplicates the most recent one. The ring is capped at
    /// [`Editor::HISTORY_LIMIT`]; once full, the oldest entry is dropped
    /// to make room for the new one.
    pub fn add_to_history(&mut self, text: &str) {
        if text.trim().is_empty() {
            self.history_index = None;
            return;
        }
        if self.history.last().is_some_and(|prev| prev == text) {
            self.history_index = None;
            return;
        }
        self.history.push(text.to_string());
        if self.history.len() > Self::HISTORY_LIMIT {
            let overflow = self.history.len() - Self::HISTORY_LIMIT;
            self.history.drain(..overflow);
        }
        self.history_index = None;
    }

    /// Insert text at the current cursor position atomically.
    ///
    /// The whole insertion is one undo unit — callers can undo it with a
    /// single Ctrl+- press, regardless of how many lines the text spans.
    /// Line endings in the input are normalized: `\r\n` and lone `\r`
    /// both become `\n`. Control characters other than `\t` are stripped
    /// (a pasted `\0` or `\x01` shouldn't land in the document); tabs
    /// collapse to a single space to match how paste mode handles them.
    ///
    /// Resets `last_action` so the insertion neither extends a previous
    /// word-typing coalesce nor leaves a stale chain open for a
    /// following yank-pop / kill accumulation.
    pub fn insert_text_at_cursor(&mut self, text: &str) {
        self.save_undo();
        // Exit history browsing; treating this like a programmatic
        // replacement of the draft.
        self.history_index = None;
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for ch in normalized.chars() {
            if ch == '\n' {
                self.insert_newline_internal();
            } else if !ch.is_control() || ch == '\t' {
                let c = if ch == '\t' { ' ' } else { ch };
                self.lines[self.cursor_line].insert(self.cursor_col, c);
                self.cursor_col += c.len_utf8();
            }
        }
        self.reset_sticky_state();
        self.last_action = LastAction::None;
        self.fire_change();
    }

    // -- Private helpers --

    fn save_undo(&mut self) {
        self.undo_stack.push(EditorSnapshot {
            lines: self.lines.clone(),
            cursor_line: self.cursor_line,
            cursor_col: self.cursor_col,
        });
    }

    fn restore_undo(&mut self) {
        if let Some(snapshot) = self.undo_stack.pop() {
            // Exit history browsing mode. `history_up` pushed a temporary
            // draft entry to the end of the history ring at entry time;
            // if we're still mid-browse when undo fires, drop that
            // temporary so the ring returns to its pre-browse shape.
            if self.history_index.is_some() {
                self.history.pop();
                self.history_index = None;
            }
            self.lines = snapshot.lines;
            self.cursor_line = snapshot.cursor_line;
            self.cursor_col = snapshot.cursor_col;
            self.reset_sticky_state();
            self.last_action = LastAction::None;
            self.fire_change();
        }
    }

    fn fire_change(&mut self) {
        if let Some(ref mut on_change) = self.on_change {
            let text = self.lines.join("\n");
            on_change(&text);
        }
    }

    fn current_line(&self) -> &str {
        &self.lines[self.cursor_line]
    }

    /// Grapheme boundaries (byte offsets) for the current line.
    fn grapheme_boundaries(&self) -> Vec<usize> {
        let line = self.current_line();
        let mut bounds = vec![0];
        for (i, _) in line.grapheme_indices(true) {
            if i > 0 {
                bounds.push(i);
            }
        }
        bounds.push(line.len());
        bounds
    }

    /// Move cursor left by one grapheme (or one paste marker), possibly
    /// wrapping to previous line.
    ///
    /// Paste markers are treated atomically: if the grapheme immediately
    /// before the cursor sits inside a marker's byte range, the cursor
    /// jumps to the marker's start in one step instead of walking
    /// grapheme-by-grapheme through the marker's internal text.
    ///
    /// Any horizontal cursor movement clears the sticky visual column
    /// used by vertical navigation.
    fn move_left(&mut self) {
        self.reset_sticky_state();
        if self.cursor_col > 0 {
            // Atomic paste-marker jump: if a marker ends exactly at the
            // cursor, step past the whole marker.
            if let Some((start, _end)) =
                marker_ending_at(self.current_line(), self.cursor_col, &self.pastes)
            {
                self.cursor_col = start;
                return;
            }
            let bounds = self.grapheme_boundaries();
            for i in (0..bounds.len()).rev() {
                if bounds[i] < self.cursor_col {
                    self.cursor_col = bounds[i];
                    return;
                }
            }
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
        }
    }

    /// Move cursor right by one grapheme (or one paste marker), possibly
    /// wrapping to next line.
    ///
    /// As with [`Editor::move_left`], any successful horizontal movement
    /// clears the sticky visual column.
    fn move_right(&mut self) {
        self.reset_sticky_state();
        let line_len = self.current_line().len();
        if self.cursor_col < line_len {
            // Atomic paste-marker jump: if a marker starts exactly at
            // the cursor, step past the whole marker.
            if let Some((_start, end)) =
                marker_starting_at(self.current_line(), self.cursor_col, &self.pastes)
            {
                self.cursor_col = end;
                return;
            }
            let bounds = self.grapheme_boundaries();
            for &b in &bounds {
                if b > self.cursor_col {
                    self.cursor_col = b;
                    return;
                }
            }
        } else if self.cursor_line < self.lines.len() - 1 {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
    }

    /// Move cursor up one visual line, honoring the sticky column and
    /// snapping atomic segments. See [`Editor::move_to_visual_line`]
    /// for the decision semantics.
    fn move_up(&mut self) {
        let width = self.layout_width.get();
        let vls = self.build_visual_line_map(width);
        if vls.is_empty() {
            return;
        }
        let current = self.find_current_visual_line(&vls);
        if current == 0 {
            return;
        }
        self.move_to_visual_line(&vls, current, current - 1);
    }

    /// Move cursor down one visual line, honoring the sticky column and
    /// snapping atomic segments. See [`Editor::move_to_visual_line`]
    /// for the decision semantics.
    fn move_down(&mut self) {
        let width = self.layout_width.get();
        let vls = self.build_visual_line_map(width);
        if vls.is_empty() {
            return;
        }
        let current = self.find_current_visual_line(&vls);
        if current + 1 >= vls.len() {
            return;
        }
        self.move_to_visual_line(&vls, current, current + 1);
    }

    /// Move the cursor by one page up (`direction = -1`) or down
    /// (`direction = 1`) within the visual-line map.
    ///
    /// Page size is `max_visible_lines`, capped at the size of the
    /// editor's visual viewport, so PageUp/PageDown jump as far as
    /// the user's currently-visible scroll window allows. The target
    /// row is clamped to the available range so paging past either
    /// edge is a no-op rather than wrapping.
    fn page_scroll(&mut self, direction: i32) {
        self.last_action = LastAction::None;
        let width = self.layout_width.get();
        let vls = self.build_visual_line_map(width);
        if vls.is_empty() {
            return;
        }
        let page_size = self.max_visible_lines().max(1);
        let current = self.find_current_visual_line(&vls);
        let target = if direction < 0 {
            current.saturating_sub(page_size)
        } else {
            (current + page_size).min(vls.len() - 1)
        };
        if target == current {
            return;
        }
        self.move_to_visual_line(&vls, current, target);
    }

    // -- Visual-line map (used by vertical cursor movement and rendering) --

    /// Build an atomic-segment list for `line` that treats every valid
    /// paste marker as a single unit and every other grapheme as its
    /// own segment. The returned segments slice into `line` directly —
    /// each `TextSegment::text` is `&line[start_index..start_index +
    /// text.len()]`.
    ///
    /// This is the per-line input the wrapping and visual-line map
    /// logic needs so that a paste marker never gets broken in half by
    /// a wrap boundary and the cursor never lands in the middle of one.
    fn segment_line<'a>(&self, line: &'a str) -> Vec<TextSegment<'a>> {
        // Fast path: no paste bookkeeping or no marker-shaped content
        // means the line is indistinguishable from its grapheme list.
        if self.pastes.is_empty() || !line.contains(PASTE_MARKER_PREFIX) {
            return line
                .grapheme_indices(true)
                .map(|(i, g)| TextSegment {
                    text: g,
                    start_index: i,
                })
                .collect();
        }

        let mut markers: Vec<(usize, usize)> = Vec::new();
        let mut scan = 0;
        while let Some((s, e, _id)) = find_next_marker(line, scan, &self.pastes) {
            markers.push((s, e));
            scan = e;
        }
        if markers.is_empty() {
            return line
                .grapheme_indices(true)
                .map(|(i, g)| TextSegment {
                    text: g,
                    start_index: i,
                })
                .collect();
        }

        let mut result: Vec<TextSegment<'a>> = Vec::new();
        let mut marker_idx = 0;
        for (i, g) in line.grapheme_indices(true) {
            // Advance past any markers that end at or before this grapheme.
            while marker_idx < markers.len() && markers[marker_idx].1 <= i {
                marker_idx += 1;
            }
            if let Some(&(ms, me)) = markers.get(marker_idx)
                && i >= ms
                && i < me
            {
                // Emit the whole marker as one segment at its start; skip
                // continuation graphemes.
                if i == ms {
                    result.push(TextSegment {
                        text: &line[ms..me],
                        start_index: ms,
                    });
                }
                continue;
            }
            result.push(TextSegment {
                text: g,
                start_index: i,
            });
        }
        result
    }

    /// Construct the visual-line map for the current document at the
    /// given layout width. Each entry records which logical line it
    /// belongs to, the byte offset into that line where the visual line
    /// starts, and the byte length of the visible span.
    ///
    /// Empty logical lines still produce one zero-length visual line.
    /// Lines that fit in `width` produce exactly one visual line
    /// spanning the full content. Wider lines are word-wrapped with
    /// paste-marker atomicity so markers never straddle a break.
    fn build_visual_line_map(&self, width: usize) -> Vec<VisualLine> {
        let width = width.max(1);
        let mut visual_lines: Vec<VisualLine> = Vec::new();

        for (i, line) in self.lines.iter().enumerate() {
            if line.is_empty() {
                visual_lines.push(VisualLine {
                    logical_line: i,
                    start_col: 0,
                    length: 0,
                });
                continue;
            }
            if visible_width(line) <= width {
                visual_lines.push(VisualLine {
                    logical_line: i,
                    start_col: 0,
                    length: line.len(),
                });
                continue;
            }
            let segments = self.segment_line(line);
            let chunks = word_wrap_line_with_segments(line, width, &segments);
            for chunk in chunks {
                visual_lines.push(VisualLine {
                    logical_line: i,
                    start_col: chunk.start_index,
                    length: chunk.end_index - chunk.start_index,
                });
            }
        }

        visual_lines
    }

    /// Return the index into `vls` of the visual line that contains the
    /// logical position `(line, col)`. For the last segment of a
    /// logical line, a cursor at exactly `length` past its start
    /// (i.e. end-of-line) also counts as contained.
    ///
    /// Falls back to the last visual line if nothing matches (should
    /// only happen if the map was built from a stale snapshot of the
    /// document — a defensive return value, not a hot path).
    fn find_visual_line_at(&self, vls: &[VisualLine], line: usize, col: usize) -> usize {
        for (i, vl) in vls.iter().enumerate() {
            if vl.logical_line != line {
                continue;
            }
            let offset = col.saturating_sub(vl.start_col);
            let is_last_segment_of_line = i == vls.len() - 1 || vls[i + 1].logical_line != line;
            if col >= vl.start_col
                && (offset < vl.length || (is_last_segment_of_line && offset == vl.length))
            {
                return i;
            }
        }
        vls.len().saturating_sub(1)
    }

    /// Return the index into `vls` of the visual line containing the
    /// editor's current cursor.
    fn find_current_visual_line(&self, vls: &[VisualLine]) -> usize {
        self.find_visual_line_at(vls, self.cursor_line, self.cursor_col)
    }

    /// Clear every piece of sticky-column state. Called from any action
    /// that is logically not a vertical cursor move (edits, horizontal
    /// moves, line-endpoint jumps, explicit `set_text`, etc.) so the
    /// next vertical move captures its own fresh sticky anchor.
    ///
    /// Keeping this single site means a new horizontal-ish operation
    /// only needs to touch one name to behave correctly — forgetting
    /// either of the two fields leaves subtle navigation bugs that
    /// are hard to reproduce.
    fn reset_sticky_state(&mut self) {
        self.preferred_visual_col = None;
        self.snapped_from_cursor_col = None;
    }

    /// Apply the sticky-column decision table and return the visual
    /// column (in bytes, into the target visual line) where the next
    /// vertical move should land.
    ///
    /// | P | S | T | U | Scenario                                             | Set Preferred | Move To     |
    /// |---|---|---|---| ---------------------------------------------------- |---------------|-------------|
    /// | 0 | * | 0 | - | Start nav, target fits                               | null          | current     |
    /// | 0 | * | 1 | - | Start nav, target shorter                            | current       | target end  |
    /// | 1 | 0 | 0 | 0 | Clamped, target fits preferred                       | null          | preferred   |
    /// | 1 | 0 | 0 | 1 | Clamped, target longer but still can't fit preferred | keep          | target end  |
    /// | 1 | 0 | 1 | - | Clamped, target even shorter                         | keep          | target end  |
    /// | 1 | 1 | 0 | - | Rewrapped, target fits current                       | null          | current     |
    /// | 1 | 1 | 1 | - | Rewrapped, target shorter than current               | current       | target end  |
    ///
    /// Where P = preferred col is set, S = cursor in middle of source
    /// line, T = target shorter than current visual col, U = target
    /// shorter than preferred col.
    fn compute_vertical_move_column(
        &mut self,
        current_visual_col: usize,
        source_max_visual_col: usize,
        target_max_visual_col: usize,
    ) -> usize {
        let has_preferred = self.preferred_visual_col.is_some();
        let cursor_in_middle = current_visual_col < source_max_visual_col;
        let target_too_short = target_max_visual_col < current_visual_col;

        if !has_preferred || cursor_in_middle {
            if target_too_short {
                // Cases 2 and 7.
                self.preferred_visual_col = Some(current_visual_col);
                return target_max_visual_col;
            }
            // Cases 1 and 6: clear preferred only. `snapped_from_cursor_col`
            // intentionally survives this path — the caller's subsequent
            // snap scan may set it, and even if not, a following vertical
            // move still needs the pre-snap anchor from the previous step.
            self.preferred_visual_col = None;
            return current_visual_col;
        }

        let preferred = self.preferred_visual_col.expect("has_preferred checked");
        let target_cant_fit_preferred = target_max_visual_col < preferred;
        if target_too_short || target_cant_fit_preferred {
            // Cases 4 and 5: keep preferred, land at target end.
            return target_max_visual_col;
        }

        // Case 3: land exactly on preferred, then clear it. Same reasoning
        // as cases 1/6 — leave `snapped_from_cursor_col` alone.
        self.preferred_visual_col = None;
        preferred
    }

    /// Move the cursor to `target_visual_line`, honoring the sticky
    /// column and snapping onto atomic segments (paste markers) that
    /// the naive column math would have landed inside.
    ///
    /// If moving down lands inside a marker that already began on a
    /// previous visual line (a "continuation" VL), the move is retried
    /// on the first VL past the marker so the cursor doesn't get stuck
    /// on the same segment it just entered.
    fn move_to_visual_line(
        &mut self,
        vls: &[VisualLine],
        current_visual_line: usize,
        target_visual_line: usize,
    ) {
        let Some(current_vl) = vls.get(current_visual_line).copied() else {
            return;
        };
        let Some(target_vl) = vls.get(target_visual_line).copied() else {
            return;
        };

        // Source visual column: use the pre-snap position if the last
        // vertical move snapped onto an atomic segment, so the decision
        // table sees the user's original intent rather than the snapped
        // offset.
        let current_visual_col = if let Some(snapped) = self.snapped_from_cursor_col {
            let vl_idx = self.find_visual_line_at(vls, current_vl.logical_line, snapped);
            snapped.saturating_sub(vls[vl_idx].start_col)
        } else {
            self.cursor_col.saturating_sub(current_vl.start_col)
        };

        // Source/target "max" columns: for non-last segments of a
        // logical line, the cursor can't sit past `length - 1` because
        // that position belongs to the next visual line. For the final
        // segment of a logical line the cursor can sit at `length`
        // (end-of-line).
        let is_last_source_segment = current_visual_line == vls.len() - 1
            || vls[current_visual_line + 1].logical_line != current_vl.logical_line;
        let source_max_visual_col = if is_last_source_segment {
            current_vl.length
        } else {
            current_vl.length.saturating_sub(1).max(0)
        };

        let is_last_target_segment = target_visual_line == vls.len() - 1
            || vls[target_visual_line + 1].logical_line != target_vl.logical_line;
        let target_max_visual_col = if is_last_target_segment {
            target_vl.length
        } else {
            target_vl.length.saturating_sub(1).max(0)
        };

        let move_to_visual_col = self.compute_vertical_move_column(
            current_visual_col,
            source_max_visual_col,
            target_max_visual_col,
        );

        self.cursor_line = target_vl.logical_line;
        let target_col = target_vl.start_col + move_to_visual_col;
        let logical_len = self.lines[target_vl.logical_line].len();
        self.cursor_col = target_col.min(logical_len);

        // Atomic-segment snap: if the cursor landed inside a multi-grapheme
        // segment (today: a paste marker), snap it to the segment start.
        // When moving down into a continuation visual line of a marker
        // that began on an earlier VL, skip forward past all the
        // continuation VLs so the cursor keeps making progress.
        let logical_line = self.lines[target_vl.logical_line].clone();
        let segments = self.segment_line(&logical_line);
        for seg in &segments {
            if seg.start_index > self.cursor_col {
                break;
            }
            if seg.text.len() <= 1 {
                continue;
            }
            let seg_end = seg.start_index + seg.text.len();
            if self.cursor_col >= seg_end {
                continue;
            }
            // Cursor is strictly inside a multi-grapheme segment.
            let is_continuation = seg.start_index < target_vl.start_col;
            let is_moving_down = target_visual_line > current_visual_line;

            if is_continuation && is_moving_down {
                // Marker started on a previous VL; skip every VL still
                // covered by this marker and try again on the first VL
                // past it.
                let mut next = target_visual_line + 1;
                while next < vls.len()
                    && vls[next].logical_line == target_vl.logical_line
                    && vls[next].start_col < seg_end
                {
                    next += 1;
                }
                if next < vls.len() {
                    self.move_to_visual_line(vls, current_visual_line, next);
                    return;
                }
            }

            // Snap to segment start and record the pre-snap cursor for
            // the next vertical move to resolve against.
            self.snapped_from_cursor_col = Some(self.cursor_col);
            self.cursor_col = seg.start_index;
            return;
        }

        // No snap — exiting whatever segment we were on.
        self.snapped_from_cursor_col = None;
    }

    /// Find byte offset of previous word boundary on current line.
    ///
    /// Thin wrapper over [`crate::word_boundary::word_boundary_left`].
    /// See that function for the three-class segmentation contract
    /// (whitespace / punctuation / word) shared with `Input`.
    fn word_boundary_left(&self) -> usize {
        crate::word_boundary::word_boundary_left(self.current_line(), self.cursor_col)
    }

    /// Whether the character immediately before the cursor on the current
    /// line is a literal `\`.
    fn cursor_preceded_by_backslash(&self) -> bool {
        if self.cursor_col == 0 {
            return false;
        }
        let line = self.current_line();
        line[..self.cursor_col].chars().next_back() == Some('\\')
    }

    /// Inverse of the standard backslash+Enter newline workaround: when
    /// the user presses `\<Enter>` and they have explicitly bound
    /// `shift+enter` (or `shift+return`) to `tui.input.submit`, the gate
    /// fires and the editor should submit instead of inserting a
    /// newline. This is the "swap config" escape hatch (Enter normally
    /// inserts a newline because the user has rebound `tui.input.newLine`
    /// to include `enter`; `\<Enter>` is the way to actually submit).
    ///
    /// Mirrors the original framework's `shouldSubmitOnBackslashEnter`.
    fn should_submit_on_backslash_enter(
        &self,
        event: &InputEvent,
        kb: &keybindings::KeybindingsManager,
    ) -> bool {
        if self.disable_submit {
            return false;
        }
        // Only plain Enter — Shift+Enter, Alt+Enter, etc. don't qualify
        // because the user explicitly chose those for newline/other actions.
        if !key_id_matches(event, "enter") {
            return false;
        }
        let submit_keys = kb.get_keys("tui.input.submit");
        let has_shift_enter = submit_keys
            .iter()
            .any(|k| k == "shift+enter" || k == "shift+return");
        if !has_shift_enter {
            return false;
        }
        self.cursor_preceded_by_backslash()
    }

    /// Reset the editor and fire `on_submit` / `on_change` callbacks for
    /// the current buffer contents. The submit value is the paste-marker-
    /// expanded text. Shared between the submit branch (`Enter`) and the
    /// newline branch's inverse workaround
    /// (`should_submit_on_backslash_enter`).
    fn submit_value(&mut self) {
        // Expand paste markers so the submitted value is the literal
        // pasted content, not the marker placeholder.
        let text = self.get_expanded_text();
        self.submitted_text = Some(text.clone());
        if let Some(ref mut on_submit) = self.on_submit {
            on_submit(&text);
        }
        // Reset the editor to an empty document. Submit is a hard break:
        // the previous content has been consumed, so the next keystroke
        // should start a fresh editing session with a clean undo stack,
        // no kill/yank/type-word chain, and the cursor at (0, 0).
        self.lines = vec![String::new()];
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.reset_sticky_state();
        self.undo_stack.clear();
        self.pastes.clear();
        self.paste_counter = 0;
        self.last_action = LastAction::None;
        self.history_index = None;
        self.fire_change();
    }

    /// Find byte offset of next word boundary on current line.
    ///
    /// Uses the same three-class model as
    /// [`crate::word_boundary::word_boundary_right`], with one
    /// editor-specific addition: if a paste marker begins immediately
    /// after the leading whitespace, the marker is skipped atomically
    /// (so `Alt+f` over `   [paste #1 +20 lines]bar` lands at the
    /// closing `]`, not after the opening `[`).
    fn word_boundary_right(&self) -> usize {
        let line = self.current_line();
        if self.cursor_col >= line.len() {
            return line.len();
        }
        let after_ws = crate::word_boundary::skip_whitespace_forward(line, self.cursor_col);
        if let Some((_start, end)) = marker_starting_at(line, after_ws, &self.pastes) {
            return end;
        }
        crate::word_boundary::skip_word_class_forward(line, after_ws)
    }

    /// Move cursor one word to the left, wrapping to the end of the
    /// previous line when already at column zero.
    fn move_word_left(&mut self) {
        if self.cursor_col == 0 {
            if self.cursor_line > 0 {
                self.cursor_line -= 1;
                self.cursor_col = self.current_line().len();
            }
            return;
        }
        self.cursor_col = self.word_boundary_left();
    }

    /// Move cursor one word to the right, wrapping to the start of the
    /// next line when already at end-of-line.
    fn move_word_right(&mut self) {
        let line_len = self.current_line().len();
        if self.cursor_col >= line_len {
            if self.cursor_line + 1 < self.lines.len() {
                self.cursor_line += 1;
                self.cursor_col = 0;
            }
            return;
        }
        self.cursor_col = self.word_boundary_right();
    }

    /// Jump the cursor to the next (or previous, for backward jumps)
    /// occurrence of `needle` in the document.
    ///
    /// Forward jumps scan from `cursor_col + 1` on the current line to
    /// end of line, then walk downward through each subsequent line
    /// starting at column 0. Backward jumps scan from `cursor_col - 1`
    /// back to column 0, then walk upward through each previous line
    /// starting at end-of-line.
    ///
    /// The search is case-sensitive and matches only the *first* byte
    /// sequence that equals the needle's UTF-8 encoding — tests assume
    /// the needle is a single ASCII codepoint, which is the intended
    /// use of this surface. A match clears
    /// `preferred_visual_col` (the vertical move no longer reflects
    /// the pre-jump line), resets `last_action`, and otherwise leaves
    /// `last_action` alone if no match is found so `Ctrl+]` followed
    /// by `z` (not found) doesn't disturb an ongoing undo chain.
    fn jump_to_char(&mut self, needle: char, direction: JumpDirection) {
        let mut buf = [0u8; 4];
        let needle_str: &str = needle.encode_utf8(&mut buf);

        match direction {
            JumpDirection::Forward => {
                // Scan current line from the codepoint *after* the
                // cursor, then each subsequent line from col 0. When
                // the cursor is already at or past end-of-line, skip
                // directly to the subsequent-line scan.
                let current = self.current_line();
                let search_start = current
                    .char_indices()
                    .find(|(i, _)| *i > self.cursor_col)
                    .map(|(i, _)| i);
                if let Some(start) = search_start
                    && let Some(rel) = current[start..].find(needle_str)
                {
                    self.cursor_col = start + rel;
                    self.reset_sticky_state();
                    self.last_action = LastAction::None;
                    return;
                }
                for line_idx in (self.cursor_line + 1)..self.lines.len() {
                    if let Some(rel) = self.lines[line_idx].find(needle_str) {
                        self.cursor_line = line_idx;
                        self.cursor_col = rel;
                        self.reset_sticky_state();
                        self.last_action = LastAction::None;
                        return;
                    }
                }
            }
            JumpDirection::Backward => {
                // Scan current line up to (not including) the cursor, then
                // each previous line in full.
                let current = self.current_line();
                if self.cursor_col > 0
                    && let Some(rel) = current[..self.cursor_col].rfind(needle_str)
                {
                    self.cursor_col = rel;
                    self.reset_sticky_state();
                    self.last_action = LastAction::None;
                    return;
                }
                for line_idx in (0..self.cursor_line).rev() {
                    if let Some(rel) = self.lines[line_idx].rfind(needle_str) {
                        self.cursor_line = line_idx;
                        self.cursor_col = rel;
                        self.reset_sticky_state();
                        self.last_action = LastAction::None;
                        return;
                    }
                }
            }
        }
        // No match: leave cursor and chain state untouched.
    }

    /// Insert a newline at cursor, splitting the current line.
    fn insert_newline_internal(&mut self) {
        // Newlines close any open autocomplete popup: the popup tracks
        // a context on the current line and a hard line break leaves
        // that context behind.
        self.cancel_autocomplete();
        let rest = self.lines[self.cursor_line][self.cursor_col..].to_string();
        self.lines[self.cursor_line].truncate(self.cursor_col);
        self.cursor_line += 1;
        self.lines.insert(self.cursor_line, rest);
        self.cursor_col = 0;
    }

    /// Insert a character at cursor.
    fn insert_char(&mut self, c: char) {
        // Undo coalescing rule (fish-style):
        //
        // - Consecutive non-whitespace characters coalesce into one undo
        //   unit. Typing "hello" pushes exactly one snapshot (before the
        //   first char).
        // - Every whitespace character pushes its own snapshot, so each
        //   space is separately undoable. Typing "hello  " pushes three:
        //   before 'h', before the first ' ', before the second ' '.
        // - Critically, the next word after whitespace does NOT push a
        //   new snapshot — `last_action` stays `TypeWord` through the
        //   whitespace. So typing "hello world" pushes two snapshots
        //   (before 'h', before ' '), and two undos take it back to
        //   empty (one removes " world", the next removes "hello").
        if c.is_whitespace() || self.last_action != LastAction::TypeWord {
            self.save_undo();
        }
        self.lines[self.cursor_line].insert(self.cursor_col, c);
        self.cursor_col += c.len_utf8();
        // Always `TypeWord` after a character insert, including for
        // whitespace — this is what prevents the next word from pushing
        // its own snapshot. The word-level snapshot boundary lives in
        // `maybe_snapshot_before_edit` based on the previous action.
        self.last_action = LastAction::TypeWord;
        self.reset_sticky_state();
        self.fire_change();
        self.maybe_trigger_autocomplete_on_insert(c);
    }

    /// Delete one grapheme backward.
    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.save_undo();
            let old_col = self.cursor_col;
            self.move_left();
            self.lines[self.cursor_line].drain(self.cursor_col..old_col);
            self.last_action = LastAction::None;
            self.reset_sticky_state();
            self.fire_change();
            self.maybe_retrigger_autocomplete_after_delete();
        } else if self.cursor_line > 0 {
            // Merge with previous line.
            self.save_undo();
            let current = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
            self.lines[self.cursor_line].push_str(&current);
            self.last_action = LastAction::None;
            self.reset_sticky_state();
            self.fire_change();
            self.maybe_retrigger_autocomplete_after_delete();
        }
    }

    /// Delete one grapheme forward. When a paste marker starts exactly
    /// at the cursor, the whole marker is deleted in one step to match
    /// its atomic navigation behavior.
    fn delete_forward(&mut self) {
        let line_len = self.current_line().len();
        if self.cursor_col < line_len {
            self.save_undo();
            let next = if let Some((_start, end)) =
                marker_starting_at(self.current_line(), self.cursor_col, &self.pastes)
            {
                end
            } else {
                let bounds = self.grapheme_boundaries();
                bounds
                    .iter()
                    .find(|&&b| b > self.cursor_col)
                    .copied()
                    .unwrap_or(line_len)
            };
            self.lines[self.cursor_line].drain(self.cursor_col..next);
            self.last_action = LastAction::None;
            self.reset_sticky_state();
            self.fire_change();
            self.maybe_retrigger_autocomplete_after_delete();
        } else if self.cursor_line < self.lines.len() - 1 {
            // Merge with next line.
            self.save_undo();
            let next = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].push_str(&next);
            self.last_action = LastAction::None;
            self.reset_sticky_state();
            self.fire_change();
            self.maybe_retrigger_autocomplete_after_delete();
        }
    }

    /// Kill from cursor to end of line.
    fn kill_to_end(&mut self) {
        let line_len = self.current_line().len();
        if self.cursor_col >= line_len {
            // At end of line -- merge with next line (kill the newline).
            if self.cursor_line < self.lines.len() - 1 {
                self.save_undo();
                let next = self.lines.remove(self.cursor_line + 1);
                self.kill_ring
                    .push("\n", false, self.last_action == LastAction::Kill);
                self.lines[self.cursor_line].push_str(&next);
                self.last_action = LastAction::Kill;
                self.fire_change();
            }
            return;
        }
        self.save_undo();
        let deleted: String = self.lines[self.cursor_line]
            .drain(self.cursor_col..)
            .collect();
        self.kill_ring
            .push(&deleted, false, self.last_action == LastAction::Kill);
        self.last_action = LastAction::Kill;
        self.fire_change();
    }

    /// Kill from cursor to start of line.
    fn kill_to_start(&mut self) {
        if self.cursor_col == 0 {
            // At start of line -- merge with previous line.
            if self.cursor_line > 0 {
                self.save_undo();
                let current = self.lines.remove(self.cursor_line);
                self.cursor_line -= 1;
                self.cursor_col = self.lines[self.cursor_line].len();
                self.kill_ring
                    .push("\n", true, self.last_action == LastAction::Kill);
                self.lines[self.cursor_line].push_str(&current);
                self.last_action = LastAction::Kill;
                self.fire_change();
            }
            return;
        }
        self.save_undo();
        let deleted: String = self.lines[self.cursor_line]
            .drain(..self.cursor_col)
            .collect();
        self.kill_ring
            .push(&deleted, true, self.last_action == LastAction::Kill);
        self.cursor_col = 0;
        self.last_action = LastAction::Kill;
        self.fire_change();
    }

    /// Kill word backward.
    ///
    /// When the cursor is at the start of a non-first line, this merges
    /// the current line into the previous one and records a `\n` in the
    /// kill ring (matching the "backspace-at-col-0" semantics but through
    /// the kill-ring path).
    fn kill_word_backward(&mut self) {
        if self.cursor_col == 0 {
            if self.cursor_line == 0 {
                return;
            }
            self.save_undo();
            let current = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
            self.lines[self.cursor_line].push_str(&current);
            self.kill_ring
                .push("\n", true, self.last_action == LastAction::Kill);
            self.last_action = LastAction::Kill;
            self.fire_change();
            return;
        }
        let target = self.word_boundary_left();
        if target == self.cursor_col {
            return;
        }
        self.save_undo();
        let deleted: String = self.lines[self.cursor_line]
            .drain(target..self.cursor_col)
            .collect();
        self.kill_ring
            .push(&deleted, true, self.last_action == LastAction::Kill);
        self.cursor_col = target;
        self.last_action = LastAction::Kill;
        self.fire_change();
    }

    /// Kill word forward.
    ///
    /// When the cursor is at the end of a non-last line, this merges the
    /// next line into the current one and records a `\n` in the kill
    /// ring (matching the "delete-at-line-end" semantics).
    fn kill_word_forward(&mut self) {
        let line_len = self.current_line().len();
        if self.cursor_col >= line_len {
            if self.cursor_line + 1 >= self.lines.len() {
                return;
            }
            self.save_undo();
            let next = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].push_str(&next);
            self.kill_ring
                .push("\n", false, self.last_action == LastAction::Kill);
            self.last_action = LastAction::Kill;
            self.fire_change();
            return;
        }
        let target = self.word_boundary_right();
        if target == self.cursor_col {
            return;
        }
        self.save_undo();
        let deleted: String = self.lines[self.cursor_line]
            .drain(self.cursor_col..target)
            .collect();
        self.kill_ring
            .push(&deleted, false, self.last_action == LastAction::Kill);
        self.last_action = LastAction::Kill;
        self.fire_change();
    }

    /// Yank from kill ring.
    fn yank(&mut self) {
        if let Some(text) = self.kill_ring.peek().map(|s| s.to_string()) {
            self.save_undo();
            self.insert_yanked_text(&text);
            self.last_action = LastAction::Yank;
            self.reset_sticky_state();
            self.fire_change();
        }
    }

    /// Cycle through the kill ring.
    ///
    /// Only valid immediately after a [`Editor::yank`] or another
    /// [`Editor::yank_pop`]: if the last action was anything else (typing,
    /// deletion, movement), bail. Also a no-op when the ring has one or
    /// zero entries — there's nothing to rotate to.
    ///
    /// Removes the text that the preceding yank (or yank-pop) inserted,
    /// rotates the ring so the next-most-recent entry becomes the head,
    /// and inserts that entry at the cursor. The undo stack sees one
    /// snapshot per yank-pop step.
    fn yank_pop(&mut self) {
        if self.last_action != LastAction::Yank || self.kill_ring.len() <= 1 {
            return;
        }
        // The yanked text is still at the ring's head because rotate
        // hasn't run yet — use it to figure out what to delete.
        let previous = match self.kill_ring.peek() {
            Some(s) => s.to_string(),
            None => return,
        };
        self.save_undo();
        self.delete_yanked_text(&previous);
        self.kill_ring.rotate();
        if let Some(next) = self.kill_ring.peek().map(|s| s.to_string()) {
            self.insert_yanked_text(&next);
        }
        self.last_action = LastAction::Yank;
        self.reset_sticky_state();
        self.fire_change();
    }

    /// Insert `text` at the cursor, handling newlines. Leaves the cursor
    /// immediately after the inserted region. Shared by yank and yank-pop
    /// so both agree exactly on the shape of the inserted region.
    fn insert_yanked_text(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.insert_newline_internal();
            } else {
                self.lines[self.cursor_line].insert(self.cursor_col, ch);
                self.cursor_col += ch.len_utf8();
            }
        }
    }

    /// Remove `text` from the document, assuming the cursor sits immediately
    /// at the end of that text (which is where [`Editor::insert_yanked_text`]
    /// last left it). Reverses the effect of the preceding yank exactly so
    /// yank-pop can replace without disturbing surrounding content.
    fn delete_yanked_text(&mut self, text: &str) {
        // Count newlines to know how many lines the yank spans.
        let yank_lines: Vec<&str> = text.split('\n').collect();
        if yank_lines.len() == 1 {
            // Single-line delete: walk backward by `text.len()` bytes on
            // the current line.
            let byte_len = text.len();
            let start_col = self.cursor_col.saturating_sub(byte_len);
            self.lines[self.cursor_line].drain(start_col..self.cursor_col);
            self.cursor_col = start_col;
            return;
        }

        // Multi-line delete: cursor is at end of the last yanked line on
        // `cursor_line`. The yank started on `cursor_line - (N - 1)` at
        // some column whose byte offset equals `start_line.len() -
        // yank_lines[0].len()` because we appended `yank_lines[0]` to the
        // start line during insertion.
        let n = yank_lines.len();
        let start_line = self.cursor_line.saturating_sub(n - 1);
        let first_yank = yank_lines.first().copied().unwrap_or("");
        let start_col = self.lines[start_line]
            .len()
            .saturating_sub(first_yank.len());

        // Text before the yank on the start line, and text after the
        // cursor on the current line, become the merged line.
        let before: String = self.lines[start_line][..start_col].to_string();
        let after: String = self.lines[self.cursor_line][self.cursor_col..].to_string();

        // Remove every line from start_line..=cursor_line and replace
        // with the merged line.
        self.lines.drain(start_line..=self.cursor_line);
        self.lines.insert(start_line, before.clone() + &after);

        self.cursor_line = start_line;
        self.cursor_col = before.len();
    }

    /// Handle paste from bracketed paste mode.
    fn handle_paste(&mut self, text: &str) {
        // A paste is a bulk replacement — any in-progress autocomplete
        // context is necessarily stale.
        self.cancel_autocomplete();
        self.save_undo();
        // Normalize line endings.
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        // Filter out control characters (except newline, which is
        // preserved, and tab, which expands to four spaces).
        let mut filtered = String::with_capacity(normalized.len());
        for ch in normalized.chars() {
            match ch {
                '\n' => filtered.push('\n'),
                '\t' => filtered.push_str("    "),
                c if c.is_control() => {}
                c => filtered.push(c),
            }
        }

        // Path-prefix safety: when the paste begins with `/`, `~`, or
        // `.` and the cursor is preceded by a word-class grapheme on
        // the current line, insert a literal space first so the paste
        // doesn't visually merge with the preceding word. Without
        // this, typing `cd` then pasting `/etc/hosts` would render as
        // `cd/etc/hosts` and read like a single token. The space goes
        // into the line buffer (not the stored paste content), which
        // means [`Editor::get_expanded_text`] also reflects the
        // separator for both inline and marker-replaced pastes.
        if matches!(filtered.chars().next(), Some('/') | Some('~') | Some('.'))
            && self.cursor_col > 0
        {
            let before = &self.lines[self.cursor_line][..self.cursor_col];
            if let Some(prev) = before.graphemes(true).next_back()
                && !is_whitespace_grapheme(prev)
                && !is_punctuation_grapheme(prev)
            {
                self.lines[self.cursor_line].insert(self.cursor_col, ' ');
                self.cursor_col += 1;
            }
        }

        // Large-paste threshold: more than 10 lines or more than 1000
        // characters is stored as a marker rather than pasted inline.
        // This keeps the editor legible when the user pastes a long
        // file, a screenful of logs, etc.
        let line_count = filtered.matches('\n').count() + 1;
        let char_count = filtered.len();
        let use_marker = line_count > 10 || char_count > 1000;

        if use_marker {
            self.paste_counter += 1;
            let id = self.paste_counter;
            self.pastes.insert(id, filtered.clone());
            let marker = if line_count > 10 {
                format!("[paste #{} +{} lines]", id, line_count)
            } else {
                format!("[paste #{} {} chars]", id, char_count)
            };
            self.lines[self.cursor_line].insert_str(self.cursor_col, &marker);
            self.cursor_col += marker.len();
        } else {
            for ch in filtered.chars() {
                if ch == '\n' {
                    self.insert_newline_internal();
                } else {
                    self.lines[self.cursor_line].insert(self.cursor_col, ch);
                    self.cursor_col += ch.len_utf8();
                }
            }
        }

        self.reset_sticky_state();
        self.last_action = LastAction::None;
        self.fire_change();
    }

    /// Return the document with every paste-marker token replaced by
    /// the original content it stands in for.
    ///
    /// Used by [`Editor::take_submitted`] so the value delivered to
    /// `on_submit` callbacks is the literal pasted text, not the
    /// marker placeholder. Callers that want the displayed text
    /// (markers and all) should use [`Editor::get_text`].
    pub fn get_expanded_text(&self) -> String {
        if self.pastes.is_empty() {
            return self.lines.join("\n");
        }
        let mut out = String::new();
        for (idx, line) in self.lines.iter().enumerate() {
            if idx > 0 {
                out.push('\n');
            }
            self.append_with_markers_expanded(&mut out, line);
        }
        out
    }

    /// Append `line` to `out`, replacing paste-marker tokens with the
    /// stored original content. Shared by [`Editor::get_expanded_text`]
    /// to avoid walking the full document when a caller only wants the
    /// expansion of one line.
    fn append_with_markers_expanded(&self, out: &mut String, line: &str) {
        let mut i = 0;
        while let Some((s, e, id)) = find_next_marker(line, i, &self.pastes) {
            out.push_str(&line[i..s]);
            if let Some(content) = self.pastes.get(&id) {
                out.push_str(content);
            } else {
                out.push_str(&line[s..e]);
            }
            i = e;
        }
        out.push_str(&line[i..]);
    }

    /// Navigate history upward.
    ///
    /// Entering history browsing (the first call when `history_index`
    /// is `None`) saves an undo snapshot of the current editor state
    /// and pushes the current text as a temporary entry at the end of
    /// the history ring. The temporary entry is what [`Editor::history_down`]
    /// will return to when walking back past the newest real entry, and
    /// what [`Editor::restore_undo`] drops when exiting history mode.
    ///
    /// Subsequent calls (already in browsing mode) do not push further
    /// snapshots — one undo should take the user back to the state
    /// before browsing started, no matter how many history entries
    /// they walked through.
    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_index {
            None => {
                // Entering history browsing mode. Save the current state
                // so a later undo restores it atomically, then push the
                // draft as the temporary tail entry.
                self.save_undo();
                self.history.push(self.get_text());
                self.history.len() - 2
            }
            Some(i) if i > 0 => i - 1,
            _ => return,
        };
        self.history_index = Some(idx);
        let text = self.history[idx].clone();
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(|s| s.to_string()).collect()
        };
        self.cursor_line = self.lines.len() - 1;
        self.cursor_col = self.lines[self.cursor_line].len();
    }

    /// Navigate history downward.
    fn history_down(&mut self) {
        let idx = match self.history_index {
            Some(i) => i + 1,
            None => return,
        };
        if idx >= self.history.len() {
            return;
        }
        self.history_index = if idx == self.history.len() - 1 {
            // Back to the temporary entry -- restore and remove it.
            None
        } else {
            Some(idx)
        };
        let text = self.history[idx].clone();
        if self.history_index.is_none() {
            self.history.pop(); // Remove temporary entry.
        }
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(|s| s.to_string()).collect()
        };
        self.cursor_line = self.lines.len() - 1;
        self.cursor_col = self.lines[self.cursor_line].len();
    }

    /// Ensure scroll offset keeps cursor visible.
    #[allow(dead_code)]
    fn adjust_scroll(&mut self, _visible_lines: usize) {
        // Build visual line map: count wrapped lines up to cursor.
        // For simplicity, use a rough approximation here.
        let max_vis = self.max_visible_lines();
        if self.cursor_line < self.scroll_offset {
            self.scroll_offset = self.cursor_line;
        }
        if self.cursor_line >= self.scroll_offset + max_vis {
            self.scroll_offset = self.cursor_line + 1 - max_vis;
        }
    }

    // -- Autocomplete state machine --

    /// Close the autocomplete popup and clear all associated state,
    /// aborting any in-flight async request. Safe to call at any time
    /// — idempotent.
    fn cancel_autocomplete(&mut self) {
        self.autocomplete_state = None;
        self.autocomplete_list = None;
        self.autocomplete_prefix.clear();
        // Drop the streaming session before the sync cancel: the
        // session's `Drop` fires its cancel token and stops the
        // walker, which is independent of the sync pipeline's own
        // in-flight task.
        self.autocomplete_session = None;
        self.cancel_pending_autocomplete_request();
    }

    /// Cancel any in-flight autocomplete request without touching the
    /// popup UI state. Used both by
    /// [`Self::cancel_autocomplete`] (which clears the UI too) and by
    /// [`Self::dispatch_autocomplete_request`] at the start of a new
    /// request.
    fn cancel_pending_autocomplete_request(&mut self) {
        if let Some(token) = self.autocomplete_cancel.take() {
            token.cancel();
        }
        // Dropping the JoinHandle does not abort the task; we rely on
        // the cancel token to short-circuit the provider work. Tests
        // may still `.await` the stored handle to sync with the task
        // terminating.
        self.autocomplete_task = None;
    }

    /// Kick off an async autocomplete request. Replaces the old
    /// inline synchronous call: this bumps the request id, cancels
    /// any prior request, snapshots the buffer, and spawns a worker
    /// that calls into the provider via `tokio::spawn`. The worker
    /// delivers results through `autocomplete_tx` and wakes the
    /// driver with `autocomplete_render_handle`.
    ///
    /// `mode` is the popup mode to apply when results arrive.
    /// `auto_apply_single` is `true` for Tab (force) dispatches: if
    /// the result has exactly one item, it is applied directly
    /// without opening the popup.
    fn dispatch_autocomplete_request(&mut self, mode: AutocompleteMode, auto_apply_single: bool) {
        let Some(provider) = self.autocomplete_provider.clone() else {
            return;
        };

        self.cancel_pending_autocomplete_request();

        self.autocomplete_request_id = self.autocomplete_request_id.wrapping_add(1);
        let request_id = self.autocomplete_request_id;
        let cancel = CancellationToken::new();
        self.autocomplete_cancel = Some(cancel.clone());

        let snapshot = AutocompleteSnapshot {
            text: self.get_text(),
            cursor_line: self.cursor_line,
            cursor_col: self.cursor_col,
        };
        let lines = self.lines.clone();
        let cursor_line = self.cursor_line;
        let cursor_col = self.cursor_col;
        let tx = self.autocomplete_tx.clone();
        let render_handle = self.autocomplete_render_handle.clone();
        let force = matches!(mode, AutocompleteMode::Force);

        // Only the `@`-attachment path gets a debounce — Tab and `/`
        // command autocomplete fire immediately. See
        // [`ATTACHMENT_AUTOCOMPLETE_DEBOUNCE`].
        let debounce = self.autocomplete_debounce_for(&snapshot, mode);

        let task = tokio::spawn(async move {
            if debounce > Duration::ZERO {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(debounce) => {}
                }
            }
            if cancel.is_cancelled() {
                return;
            }

            let opts = SuggestOpts {
                cancel: cancel.clone(),
                force,
            };
            let suggestions = provider
                .get_suggestions(&lines, cursor_line, cursor_col, opts)
                .await;

            if cancel.is_cancelled() {
                return;
            }

            // Channel send is fire-and-forget; a closed receiver
            // means the editor is gone and we have nothing useful
            // to do.
            let _ = tx.send(AutocompleteDelivery {
                request_id,
                snapshot,
                suggestions,
                mode,
                auto_apply_single,
            });
            render_handle.request_render();
        });

        self.autocomplete_task = Some(task);
    }

    /// Return the debounce interval appropriate for the given request
    /// context. `@`-attachment triggers get a small coalescing window
    /// to absorb rapid keystrokes into one walk; everything else is
    /// immediate.
    fn autocomplete_debounce_for(
        &self,
        snapshot: &AutocompleteSnapshot,
        mode: AutocompleteMode,
    ) -> Duration {
        if matches!(mode, AutocompleteMode::Force) {
            return Duration::ZERO;
        }
        // Match pi's attachment-context detection: if the word-boundary
        // token at the cursor starts with `@`, debounce.
        let line = snapshot
            .text
            .lines()
            .nth(snapshot.cursor_line)
            .unwrap_or("");
        let before = line
            .char_indices()
            .take(snapshot.cursor_col)
            .map(|(_, c)| c)
            .collect::<String>();
        let token_start = before
            .rfind(|c: char| matches!(c, ' ' | '\t' | '"' | '\'' | '='))
            .map(|i| i + 1)
            .unwrap_or(0);
        let token = &before[token_start..];
        if token.starts_with('@') {
            ATTACHMENT_AUTOCOMPLETE_DEBOUNCE
        } else {
            Duration::ZERO
        }
    }

    /// Drain any pending autocomplete results, applying the most
    /// recent non-stale one to the popup state. Called once per
    /// frame from [`Component::tick`] and (defensively) at the top
    /// of [`Component::handle_input`]. Fire-and-forget — never
    /// blocks, never waits for new results.
    ///
    /// Also pumps any active streaming session: if nucleo has new
    /// matches ready, the popup's list is rebuilt here so the next
    /// render sees the updated snapshot.
    fn drain_autocomplete_results(&mut self) {
        // Streaming session: tick + refresh the visible list.
        // Happens first because a live session makes pending one-
        // shot deliveries moot — they'd be discarded anyway by the
        // snapshot guard in `apply_autocomplete_delivery`, and
        // we'd rather let the session drive the popup without
        // interference.
        self.pump_autocomplete_session();

        // Drain *everything* from the channel, keeping only the last
        // delivery whose request id matches the current one. This is
        // cheaper than applying intermediate results we're about to
        // overwrite, and also correct when multiple worker tasks have
        // landed between frames.
        let mut latest: Option<AutocompleteDelivery> = None;
        while let Ok(delivery) = self.autocomplete_rx.try_recv() {
            if delivery.request_id != self.autocomplete_request_id {
                continue;
            }
            latest = Some(delivery);
        }
        if let Some(delivery) = latest {
            // If a streaming session took over since this request
            // was spawned, the delivery is stale in spirit — don't
            // let it clobber the session's list.
            if self.autocomplete_session.is_none() {
                self.apply_autocomplete_delivery(delivery);
            }
        }
    }

    /// Apply a single delivery after all the staleness guards have
    /// passed. Factored out from [`Self::drain_autocomplete_results`]
    /// so it can also be unit-tested directly.
    fn apply_autocomplete_delivery(&mut self, delivery: AutocompleteDelivery) {
        // Final guard: the buffer may have changed since the task was
        // spawned even though the request id still matches (e.g. the
        // task itself ran for longer than debounce and the user kept
        // typing but within the same request id — hasn't happened, but
        // guarding here is cheap insurance).
        let current = AutocompleteSnapshot {
            text: self.get_text(),
            cursor_line: self.cursor_line,
            cursor_col: self.cursor_col,
        };
        if current != delivery.snapshot {
            return;
        }

        let Some(suggestions) = delivery.suggestions else {
            // In Force mode with no matches, keep the popup open but
            // empty so the user can continue narrowing; in Regular
            // mode, close it.
            if matches!(delivery.mode, AutocompleteMode::Force) {
                self.autocomplete_state = Some(AutocompleteMode::Force);
                self.autocomplete_list = None;
                self.autocomplete_prefix.clear();
            } else {
                self.autocomplete_state = None;
                self.autocomplete_list = None;
                self.autocomplete_prefix.clear();
            }
            return;
        };

        if delivery.auto_apply_single && suggestions.items.len() == 1 {
            let item = suggestions.items[0].clone();
            self.autocomplete_prefix = suggestions.prefix;
            self.apply_autocomplete_item(item);
            return;
        }

        if suggestions.items.is_empty() {
            if !matches!(delivery.mode, AutocompleteMode::Force) {
                self.autocomplete_state = None;
                self.autocomplete_list = None;
                self.autocomplete_prefix.clear();
            }
            return;
        }

        self.autocomplete_prefix = suggestions.prefix.clone();
        let items: Vec<SelectItem> = suggestions.items.iter().map(Self::item_to_select).collect();
        let highlight_idx = suggestions
            .items
            .iter()
            .position(|it| self.item_matches_typed_prefix(it))
            .unwrap_or(0);

        // Construct the popup via the prefix-aware helper. Mirrors
        // pi-tui's `applyAutocompleteSuggestions` (`editor.ts:2237-2247`):
        // build the SelectList through `createAutocompleteList`, then
        // set the best-match selection and the active mode.
        let mut list = self.create_autocomplete_list(&self.autocomplete_prefix, items);
        list.set_selected_index(highlight_idx);
        self.autocomplete_list = Some(list);
        self.autocomplete_state = Some(delivery.mode);
    }

    /// Public draining entry point for tests that drive the editor
    /// synchronously: block the current task until the in-flight
    /// autocomplete work (one-shot task *and* streaming walker)
    /// completes, then drain its result. Must be called from within
    /// a tokio runtime.
    ///
    /// Handles both autocomplete shapes:
    ///
    /// - **One-shot:** awaits the stored `autocomplete_task`
    ///   handle, then drains the delivery channel.
    /// - **Streaming session:** awaits the walker task, then
    ///   spins on `tick` until nucleo reports a quiescent status
    ///   (not running, not changed). This guarantees the session's
    ///   snapshot is final before the test inspects
    ///   `autocomplete_list`.
    ///
    /// No-op if there is no pending work.
    pub async fn wait_for_pending_autocomplete(&mut self) {
        if let Some(handle) = self.autocomplete_task.take() {
            // Best-effort await; an aborted or panicked task is a
            // no-op from the editor's point of view.
            let _ = handle.await;
        }
        // Streaming path: spin on `tick` until nucleo reports a
        // quiescent status. This captures both the walker's state
        // (its live injector keeps `active_injectors > 0` while
        // it's pushing) and nucleo's own work queue. The walker
        // runs on `spawn_blocking`'s dedicated thread pool and
        // nucleo has its own rayon pool, so both are making
        // progress while we yield here.
        if self.autocomplete_session.is_some() {
            for _ in 0..500 {
                tokio::task::yield_now().await;
                let Some(session) = self.autocomplete_session.as_mut() else {
                    break;
                };
                let status = session.tick(20);
                if !status.running {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        }
        self.drain_autocomplete_results();
    }

    /// Ask the installed provider for suggestions at the current cursor
    /// and update the popup state. `force = true` drives the Tab path
    /// (explicit user request, stays open on narrow); `force = false`
    /// drives implicit triggers from `@` / `/` typing.
    ///
    /// # Streaming-first
    ///
    /// Before dispatching a one-shot async request, the editor tries
    /// to drive its existing streaming session (if any) or open a
    /// new one through
    /// [`AutocompleteProvider::try_start_session`]. Streaming
    /// providers handle `@`-fuzzy-file completion in-process via
    /// [`nucleo::Nucleo`]: keystrokes only update the matcher
    /// pattern, not re-walk the filesystem. The one-shot path stays
    /// available for everything else (slash commands, direct path
    /// completion) and for providers that don't implement the
    /// streaming API.
    ///
    /// Asynchronous on the one-shot path: returns immediately after
    /// dispatching the request. The popup will appear (or
    /// disappear) once the worker delivers results — normally a few
    /// milliseconds later, or tens of ms for a large walk.
    fn update_autocomplete(&mut self, force: bool) {
        let Some(provider) = self.autocomplete_provider.clone() else {
            return;
        };

        let mode = if force {
            AutocompleteMode::Force
        } else {
            // Preserve an existing Force mode across refresh-while-
            // typing so a narrowing keystroke doesn't downgrade the
            // popup to Regular and change close semantics.
            match self.autocomplete_state {
                Some(AutocompleteMode::Force) => AutocompleteMode::Force,
                _ => AutocompleteMode::Regular,
            }
        };

        // Path 1: an existing streaming session. Try to absorb the
        // new cursor state. On success we're done; on failure we
        // drop it and fall through to try opening a fresh session.
        if let Some(session) = self.autocomplete_session.as_mut() {
            match session.update(&self.lines, self.cursor_line, self.cursor_col) {
                Ok(()) => {
                    self.autocomplete_prefix = session.prefix().to_string();
                    return;
                }
                Err(SessionInvalid) => {
                    self.autocomplete_session = None;
                }
            }
        }

        // Path 2: open a new streaming session for the current
        // context. If the provider opts in, we skip the sync
        // dispatch entirely.
        let notify = self.make_autocomplete_notify();
        if let Some(session) =
            provider.try_start_session(&self.lines, self.cursor_line, self.cursor_col, notify)
        {
            // Close any in-flight one-shot request: with a session
            // in charge, its delivery would never apply anyway.
            self.cancel_pending_autocomplete_request();
            self.autocomplete_prefix = session.prefix().to_string();
            self.autocomplete_session = Some(session);
            self.autocomplete_state = Some(mode);
            // The list is populated on the next
            // `pump_autocomplete_session` call once nucleo has some
            // matches to show. Clearing it here keeps the popup
            // from displaying a stale snapshot from a previous
            // session.
            self.autocomplete_list = None;
            return;
        }

        // Path 3: fall back to the one-shot async pipeline.
        self.dispatch_autocomplete_request(mode, false);
    }

    /// Build the notify callback that we hand to a streaming
    /// session. The callback nudges the `Tui`'s render scheduler so
    /// the popup repaints as matches stream in. Wrapped in `Arc<dyn
    /// Fn>` because nucleo's API requires `Send + Sync + 'static`.
    fn make_autocomplete_notify(&self) -> std::sync::Arc<dyn Fn() + Send + Sync> {
        let handle = self.autocomplete_render_handle.clone();
        std::sync::Arc::new(move || {
            handle.request_render();
        })
    }

    /// Pump any active streaming session: give its matcher a small
    /// slice of time to absorb queued work, then rebuild
    /// `autocomplete_list` if the snapshot changed.
    ///
    /// Idempotent and safe to call on every drain.
    fn pump_autocomplete_session(&mut self) {
        let Some(session) = self.autocomplete_session.as_mut() else {
            return;
        };
        let status = session.tick(10);
        let needs_rebuild = status.changed || self.autocomplete_list.is_none();
        if !needs_rebuild {
            return;
        }

        let items_from_snap = session.snapshot();

        if items_from_snap.is_empty() {
            // No matches. If the walker is still running we may
            // see matches later — leave the popup open with an
            // empty list. If not, close it in Regular mode (so a
            // stray `@xyz` that matches nothing doesn't linger);
            // leave Force mode open so the user can keep
            // narrowing.
            if !status.running && matches!(self.autocomplete_state, Some(AutocompleteMode::Regular))
            {
                self.cancel_autocomplete();
            } else {
                self.autocomplete_list = None;
            }
            return;
        }

        let items: Vec<SelectItem> = items_from_snap.iter().map(Self::item_to_select).collect();
        let highlight_idx = items_from_snap
            .iter()
            .position(|it| self.item_matches_typed_prefix(it))
            .unwrap_or(0);

        // Same `create_autocomplete_list` helper as the one-shot path
        // above. The streaming-session path has no pi equivalent (pi
        // only has a single one-shot path through
        // `applyAutocompleteSuggestions`), but routing it through the
        // same helper keeps the popup-construction contract symmetric.
        let mut list = self.create_autocomplete_list(&self.autocomplete_prefix, items);
        list.set_selected_index(highlight_idx);
        self.autocomplete_list = Some(list);
    }

    /// Convert an autocomplete item to the SelectList's item shape.
    fn item_to_select(item: &AutocompleteItem) -> SelectItem {
        let mut s = SelectItem::new(&item.value, &item.label);
        if let Some(desc) = &item.description {
            s = s.with_description(desc);
        }
        s
    }

    /// Slash-command autocomplete popup layout. Mirrors pi-tui's
    /// `SLASH_COMMAND_SELECT_LIST_LAYOUT` (`editor.ts:210-213`). The
    /// `[12, 32]` bounds give short commands more breathing room for the
    /// description column without losing the cap on long names.
    ///
    /// A function rather than a `const` because [`SelectListLayout`] carries
    /// an `Option<Box<dyn Fn>>` field for `truncate_primary`, which isn't
    /// `const`-compatible.
    fn slash_command_select_list_layout() -> SelectListLayout {
        SelectListLayout {
            min_primary_column_width: Some(12),
            max_primary_column_width: Some(32),
            truncate_primary: None,
        }
    }

    /// Build the autocomplete popup's [`SelectList`]. Mirrors pi-tui's
    /// `createAutocompleteList(prefix, items)` (`editor.ts:2074-2080`)
    /// byte-for-byte: slash-command popups get the tighter `[12, 32]`
    /// primary-column bounds via [`Self::slash_command_select_list_layout`];
    /// every other trigger (`@`-style file completion, etc.) takes the
    /// layout default (a fixed 32-cell primary column).
    ///
    /// The popup's theme is cloned from `self.theme.select_list`. Because
    /// [`SelectListTheme`] derives `Clone` (its closures are
    /// `Arc<dyn Fn>`), this is a cheap refcount bump — no rebuilding of
    /// the closures and no silent drop of the agent's configured palette.
    /// Caller is responsible for any post-construction state (selected
    /// index, autocomplete mode, etc.); this helper only constructs the
    /// list, mirroring pi's split between `createAutocompleteList` and
    /// `applyAutocompleteSuggestions`.
    fn create_autocomplete_list(&self, prefix: &str, items: Vec<SelectItem>) -> SelectList {
        let layout = if prefix.starts_with('/') {
            Self::slash_command_select_list_layout()
        } else {
            SelectListLayout::default()
        };
        SelectList::new(
            items,
            self.autocomplete_max_visible,
            self.theme.select_list.clone(),
            layout,
        )
    }

    /// Whether the suggestion item's value begins with the typed text
    /// at the cursor — used to pick the pre-highlighted list entry
    /// so a unique prefix match lights up without further navigation.
    fn item_matches_typed_prefix(&self, item: &AutocompleteItem) -> bool {
        let line = &self.lines[self.cursor_line];
        let prefix_len = self.autocomplete_prefix.len();
        let typed_start = self.cursor_col.saturating_sub(prefix_len);
        let typed = &line[typed_start..self.cursor_col];
        item.value.starts_with(typed)
    }

    /// Apply the autocomplete item to the buffer: splice its `value`
    /// over the `autocomplete_prefix`-long span ending at the cursor.
    /// Resets sticky state, pushes an undo snapshot, and closes the
    /// popup.
    fn apply_autocomplete_item(&mut self, item: AutocompleteItem) {
        let Some(provider) = &self.autocomplete_provider else {
            return;
        };
        let prefix = self.autocomplete_prefix.clone();
        let result = provider.apply_completion(
            &self.lines,
            self.cursor_line,
            self.cursor_col,
            &item,
            &prefix,
        );
        self.save_undo();
        self.lines = result.lines;
        self.cursor_line = result.cursor_line;
        self.cursor_col = result.cursor_col;
        self.cancel_autocomplete();
        self.reset_sticky_state();
        self.last_action = LastAction::None;
        self.fire_change();
    }

    /// Entry point for Tab when no popup is visible: request a forced
    /// suggestion set asynchronously. If exactly one result comes back,
    /// it is applied immediately without opening the popup; if
    /// multiple, the popup opens in Force mode; otherwise nothing
    /// happens.
    ///
    /// Asynchronous: control returns before the worker has produced a
    /// result, so the effect is visible on the next frame.
    fn trigger_force_autocomplete(&mut self) {
        self.dispatch_autocomplete_request(AutocompleteMode::Force, true);
    }

    /// Called by typing-paths (insert_char, backspace, etc.) to check
    /// whether to trigger a new autocomplete request or update an
    /// existing one. The provider decides whether any suggestions
    /// apply at the current context.
    #[allow(dead_code)]
    fn maybe_update_autocomplete_after_edit(&mut self) {
        // Kept as a backward-compatibility shim in case a future caller
        // needs the old "just refresh if open" semantics without knowing
        // which character drove the edit. All current edit paths route
        // through the narrower helpers below.
        self.maybe_retrigger_autocomplete_after_delete();
    }

    /// Entry point called after inserting a single `char`.
    ///
    /// Opens the autocomplete popup only when the just-typed character
    /// plausibly starts or continues a completable context:
    ///
    /// - `/` at the start of the first line → slash-command popup
    /// - `@` immediately after whitespace or start-of-line → `@`-file popup
    /// - `[A-Za-z0-9._-]` while the cursor is already inside a slash or
    ///   `@` context → refines the open popup
    ///
    /// For any other character (including plain alphabetical letters
    /// outside of a completion context, and crucially whitespace), no
    /// new popup opens. If a popup is already visible, every insert
    /// refreshes it, preserving the narrowing behavior.
    fn maybe_trigger_autocomplete_on_insert(&mut self, c: char) {
        if self.autocomplete_provider.is_none() {
            return;
        }
        // Already open: keep refreshing so the user can narrow.
        if self.autocomplete_state.is_some() {
            let force = matches!(self.autocomplete_state, Some(AutocompleteMode::Force));
            self.update_autocomplete(force);
            return;
        }
        let should_trigger = match c {
            '/' => self.is_at_start_of_message(),
            '@' => self.at_sign_follows_whitespace_or_start(),
            c if is_identifier_char(c) => {
                self.is_in_slash_command_context() || self.is_in_at_context()
            }
            _ => false,
        };
        if should_trigger {
            self.update_autocomplete(false);
        }
    }

    /// Entry point called after a deletion (backspace, forward-delete,
    /// kill). If a popup is already open, refresh it against the new
    /// buffer; otherwise, re-trigger only if the deletion has left the
    /// cursor inside a slash or `@` context.
    fn maybe_retrigger_autocomplete_after_delete(&mut self) {
        if self.autocomplete_provider.is_none() {
            return;
        }
        if self.autocomplete_state.is_some() {
            let force = matches!(self.autocomplete_state, Some(AutocompleteMode::Force));
            self.update_autocomplete(force);
            return;
        }
        if self.is_in_slash_command_context() || self.is_in_at_context() {
            self.update_autocomplete(false);
        }
    }

    /// Slash menus only make sense on the first line of a message: when
    /// the user is typing a prose reply across multiple lines, a `/` in
    /// the middle of line 2 is almost certainly part of a path or a
    /// regex, not a command.
    fn is_slash_menu_allowed(&self) -> bool {
        self.cursor_line == 0
    }

    /// True when the cursor is positioned on a line that's empty or
    /// contains only `/`, ignoring leading/trailing whitespace. Used
    /// to decide whether a freshly-typed `/` opens a slash-command
    /// popup.
    fn is_at_start_of_message(&self) -> bool {
        if !self.is_slash_menu_allowed() {
            return false;
        }
        let before = &self.lines[self.cursor_line][..self.cursor_col];
        let trimmed = before.trim();
        trimmed.is_empty() || trimmed == "/"
    }

    /// Called right after inserting `@`: returns true iff the character
    /// before the just-inserted `@` is whitespace or the `@` is at the
    /// start of the line. Mirrors the regex `(?:^|[ \t])@$` guard in the
    /// insert-char path: `@foo` in the middle of a word shouldn't open
    /// the fuzzy-file popup.
    fn at_sign_follows_whitespace_or_start(&self) -> bool {
        let before = &self.lines[self.cursor_line][..self.cursor_col];
        let Some(before_at) = before.strip_suffix('@') else {
            return false;
        };
        match before_at.chars().last() {
            None => true,
            Some(c) => c == ' ' || c == '\t',
        }
    }

    /// True when the text before the cursor looks like a slash-command
    /// line (`/` as the first non-whitespace character, after trimming
    /// leading whitespace).
    fn is_in_slash_command_context(&self) -> bool {
        if !self.is_slash_menu_allowed() {
            return false;
        }
        let before = &self.lines[self.cursor_line][..self.cursor_col];
        before.trim_start().starts_with('/')
    }

    /// True when the cursor is inside an `@`-file reference: the
    /// rightmost `@` in the text before the cursor is preceded by
    /// whitespace or start-of-line, and no whitespace appears between
    /// that `@` and the cursor. Equivalent to the regex
    /// `(?:^|[\s])@[^\s]*$` applied to the text before the cursor.
    fn is_in_at_context(&self) -> bool {
        let before = &self.lines[self.cursor_line][..self.cursor_col];
        let Some(at_byte_idx) = before.rfind('@') else {
            return false;
        };
        let after_at = &before[at_byte_idx + '@'.len_utf8()..];
        if after_at.chars().any(char::is_whitespace) {
            return false;
        }
        if at_byte_idx == 0 {
            return true;
        }
        before[..at_byte_idx]
            .chars()
            .last()
            .is_some_and(char::is_whitespace)
    }

    /// Apply whichever autocomplete item the current popup resolves
    /// to for Enter. The rule: if the typed prefix is an exact match
    /// for any item, keep the typed value literally. Otherwise, apply
    /// the first item whose value starts with the
    /// typed prefix (the highlighted one).
    ///
    /// Returns [`EnterOutcome`]:
    ///
    /// - `Consumed` — completion applied (or popup dismissed) and the
    ///   caller should treat the Enter as done.
    /// - `FallThroughToSubmit` — completion applied *and* the prefix
    ///   was a slash-command name (`/foo`), so the caller should also
    ///   run its Enter-submits-the-message handler. This matches the
    ///   convention where pressing Enter on `/clear` both writes the
    ///   literal `/clear` into the buffer and immediately submits it
    ///   as a command.
    fn accept_autocomplete_on_enter(&mut self) -> EnterOutcome {
        let Some(list) = &self.autocomplete_list else {
            return EnterOutcome::Consumed;
        };
        let Some(selected) = list.selected_item().cloned() else {
            self.cancel_autocomplete();
            return EnterOutcome::Consumed;
        };
        // Determine the typed argument at the cursor.
        let line = &self.lines[self.cursor_line];
        let prefix_len = self.autocomplete_prefix.len();
        let typed_start = self.cursor_col.saturating_sub(prefix_len);
        let typed = line[typed_start..self.cursor_col].to_string();

        // Whether this popup is completing a slash-command *name*
        // (prefix like `/clear`) as opposed to a slash-command
        // argument (prefix = the argument text), an `@`-file ref
        // (prefix like `@src/`), or a raw path. Only the slash-
        // command-name case triggers submit-after-accept.
        let prefix_is_slash_command = self.autocomplete_prefix.starts_with('/');

        // If the typed value is exactly one of the item values, keep
        // it verbatim and just close.
        let exact = list.items().iter().any(|i| i.value == typed);
        if exact {
            self.cancel_autocomplete();
        } else {
            self.apply_autocomplete_item(AutocompleteItem {
                value: selected.value,
                label: selected.label,
                description: selected.description,
            });
        }

        if prefix_is_slash_command {
            EnterOutcome::FallThroughToSubmit
        } else {
            EnterOutcome::Consumed
        }
    }
}

impl Component for Editor {
    crate::impl_component_any!();

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }

    fn render(&mut self, width: usize) -> Vec<String> {
        // Drain any autocomplete deliveries that arrived since the
        // previous frame. The async worker's send resumes us here, on
        // the editor's turn to read its state; apply the latest
        // snapshot before anyone else reads `autocomplete_state`.
        self.drain_autocomplete_results();

        let content_width = width.saturating_sub(self.padding_x * 2);
        if content_width == 0 {
            return Vec::new();
        }

        // Layout width: when padding_x is zero we reserve one column at
        // the right edge for the cursor marker. Otherwise the cursor can
        // overflow into the padding. Without this reservation, a line
        // that fills content_width exactly would render the cursor
        // marker one column past the right edge.
        let layout_width = if self.padding_x == 0 {
            content_width.saturating_sub(1).max(1)
        } else {
            content_width
        };

        // Remember the width we laid out at so vertical cursor movement
        // that happens between renders can rebuild an accurate visual-
        // line map. Render is `&self`, so we stash this through the
        // `Cell`. Navigation before the first render falls back to the
        // constructor default.
        self.layout_width.set(layout_width);

        let left_padding = " ".repeat(self.padding_x);
        let right_padding = left_padding.clone();
        let max_vis = self.max_visible_lines();

        // Build visual lines from logical lines with word wrapping.
        struct VisualLine {
            text: String,
            #[allow(dead_code)]
            logical_line: usize,
            has_cursor: bool,
            cursor_vis_col: Option<usize>,
        }

        let mut visual_lines: Vec<VisualLine> = Vec::new();

        for (line_idx, line) in self.lines.iter().enumerate() {
            let wrapped = if line.is_empty() {
                vec![String::new()]
            } else {
                wrap_text_with_ansi(line, layout_width)
            };

            // Find which wrapped line contains the cursor.
            let cursor_on_this_line = line_idx == self.cursor_line;
            let mut cursor_byte_remaining = if cursor_on_this_line {
                Some(self.cursor_col)
            } else {
                None
            };

            let mut chunk_start = 0;
            for (i, wrapped_line) in wrapped.iter().enumerate() {
                let chunk_end = if i < wrapped.len() - 1 {
                    // Estimate: chunk covers approximately this much of the original.
                    chunk_start + wrapped_line.len()
                } else {
                    line.len()
                };

                let mut has_cursor = false;
                let mut cursor_vis_col = None;

                if let Some(ref mut remaining) = cursor_byte_remaining {
                    if *remaining <= chunk_end.saturating_sub(chunk_start) || i == wrapped.len() - 1
                    {
                        has_cursor = true;
                        // Approximate visual column.
                        let col_in_chunk = (*remaining).min(visible_width(wrapped_line));
                        cursor_vis_col = Some(col_in_chunk);
                        cursor_byte_remaining = None;
                    } else {
                        *remaining -= chunk_end - chunk_start;
                    }
                }

                visual_lines.push(VisualLine {
                    text: wrapped_line.clone(),
                    logical_line: line_idx,
                    has_cursor,
                    cursor_vis_col,
                });
                chunk_start = chunk_end;
            }
        }

        // Find which visual line has the cursor.
        let cursor_visual_idx = visual_lines
            .iter()
            .position(|vl| vl.has_cursor)
            .unwrap_or(0);

        // Compute scroll window.
        let total_visual = visual_lines.len();
        let visible_count = total_visual.min(max_vis);
        let mut scroll_start = if cursor_visual_idx < self.scroll_offset {
            cursor_visual_idx
        } else if cursor_visual_idx >= self.scroll_offset + visible_count {
            cursor_visual_idx + 1 - visible_count
        } else {
            self.scroll_offset
        };
        scroll_start = scroll_start.min(total_visual.saturating_sub(visible_count));

        let mut result = Vec::new();

        // The hardware cursor marker is suppressed while an autocomplete
        // popup is visible: the focus-visible cursor belongs to the
        // popup in that case, so emitting a marker in the editor would
        // fight the select list for the hardware cursor position.
        let emit_cursor_marker = self.focused && self.autocomplete_state.is_none();

        // Top border. When the buffer is scrolled we show a scroll
        // indicator like `─── ↑ N more ` followed by `─` padding to the
        // end of the line; otherwise a plain `─` line spanning `width`.
        let horizontal = (self.theme.border_color)("─");
        if scroll_start > 0 {
            let indicator = format!("─── ↑ {} more ", scroll_start);
            let ind_w = visible_width(&indicator);
            let line = if ind_w >= width {
                truncate_to_width(&indicator, width, "", false)
            } else {
                format!("{}{}", indicator, "─".repeat(width - ind_w))
            };
            result.push((self.theme.border_color)(&line));
        } else {
            result.push(horizontal.repeat(width));
        }

        // Visible content.
        for i in scroll_start..scroll_start + visible_count {
            if i >= visual_lines.len() {
                break;
            }
            let vl = &visual_lines[i];
            let mut line_text = vl.text.clone();
            let mut line_visible_width = visible_width(&line_text);
            let mut cursor_in_padding = false;

            // Render cursor.
            if vl.has_cursor && self.focused {
                let col = vl.cursor_vis_col.unwrap_or(0);
                let graphemes: Vec<&str> = line_text.graphemes(true).collect();
                let mut new_line = String::new();
                let mut vis_col = 0;
                let mut cursor_placed = false;

                for g in &graphemes {
                    let gw = visible_width(g);
                    if vis_col == col && !cursor_placed {
                        if emit_cursor_marker {
                            new_line.push_str(CURSOR_MARKER);
                        }
                        // Cursor cell terminates with a full SGR reset
                        // (`\x1b[0m`), not just reverse-video-off
                        // (`\x1b[27m`). The full reset closes any styling
                        // open in the surrounding text (e.g. an
                        // ANSI-colored line whose color escape sits
                        // before the cursor cell) so the styling does
                        // not bleed into the cells that follow on the
                        // same row.
                        new_line.push_str(&format!("\x1b[7m{}\x1b[0m", g));
                        cursor_placed = true;
                    } else {
                        new_line.push_str(g);
                    }
                    vis_col += gw;
                }
                if !cursor_placed {
                    // Cursor at end of line: emit a highlighted space.
                    if emit_cursor_marker {
                        new_line.push_str(CURSOR_MARKER);
                    }
                    // Same `\x1b[0m` (full SGR reset) rationale as the
                    // grapheme-cursor branch above.
                    new_line.push_str("\x1b[7m \x1b[0m");
                    line_visible_width += 1;
                    // If the highlighted-space cursor pushed the line
                    // past `content_width`, the cursor cell sits inside
                    // the right-side padding column. In that case the
                    // right pad is one space short so the total visible
                    // width still matches `width`.
                    if line_visible_width > content_width && self.padding_x > 0 {
                        cursor_in_padding = true;
                    }
                }
                line_text = new_line;
            }

            // Right-pad to `content_width` (inner pad), then emit the
            // configured `right_padding` (or one space short of it
            // when the cursor occupies a padding cell). Without these
            // tail pads, a row whose text is shorter than the
            // editor's content area leaves the right-side cells
            // untouched — and on a render whose terminal background
            // differs from the editor's, those cells display the
            // wrong color until the next full repaint.
            let inner_pad = content_width.saturating_sub(line_visible_width);
            let tail = if cursor_in_padding {
                &right_padding[..right_padding.len().saturating_sub(1)]
            } else {
                right_padding.as_str()
            };
            result.push(format!(
                "{}{}{}{}",
                left_padding,
                line_text,
                " ".repeat(inner_pad),
                tail,
            ));
        }

        // Bottom border, with a `↓ N more ` indicator when content
        // extends below the visible slice.
        let lines_below = total_visual.saturating_sub(scroll_start + visible_count);
        if lines_below > 0 {
            let indicator = format!("─── ↓ {} more ", lines_below);
            let ind_w = visible_width(&indicator);
            let line = if ind_w >= width {
                truncate_to_width(&indicator, width, "", false)
            } else {
                format!("{}{}", indicator, "─".repeat(width - ind_w))
            };
            result.push((self.theme.border_color)(&line));
        } else {
            result.push(horizontal.repeat(width));
        }

        // Autocomplete popup: if the state machine has an open list,
        // render it directly below the editor's bottom border. Each
        // popup row gets the same `left_padding + content +
        // inner-pad-to-content-width + right_padding` shape as a
        // visible editor row, so the whole component renders as a
        // single uniform-width column.
        if self.autocomplete_state.is_some() {
            if let Some(list) = self.autocomplete_list.as_mut() {
                let content_width = width.saturating_sub(self.padding_x * 2);
                for line in list.render(content_width) {
                    let line_w = visible_width(&line);
                    let inner_pad = content_width.saturating_sub(line_w);
                    result.push(format!(
                        "{}{}{}{}",
                        left_padding,
                        line,
                        " ".repeat(inner_pad),
                        right_padding,
                    ));
                }
            }
        }

        result
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        // Consume any autocomplete deliveries that landed since the
        // previous render. Without this, a delivery arriving between
        // two keystrokes (common for fast fuzzy walks) would only
        // be visible after the *next* render, making the popup feel
        // laggy even though the worker finished in time.
        self.drain_autocomplete_results();
        match event {
            InputEvent::Paste(text) => {
                self.handle_paste(text);
                true
            }
            InputEvent::Key(key) => {
                let mods = key.modifiers;

                // Character-jump mode intercept. Once set by Ctrl+]
                // or Ctrl+Alt+], the next input either:
                // - cancels the mode (a second Ctrl+]/Ctrl+Alt+]
                //   matching the direction, consumed by the editor),
                // - performs the jump (any other printable char), or
                // - cancels silently and falls through (any
                //   non-printable input — Esc, arrow keys, Shift
                //   alone, the user's bound `tui.select.cancel`,
                //   etc.). Pi-tui parity: jump-mode does not consult
                //   `tui.select.cancel` (see editor.ts:534-556).
                //   Control characters drop through so the parent
                //   surface can handle Esc / Ctrl+C / etc. itself.
                if let Some(direction) = self.jump_mode {
                    let kb = keybindings::get();
                    // Second press of the binding for the same direction
                    // cancels.
                    let cancels_direction = match direction {
                        JumpDirection::Forward => kb.matches(event, "tui.editor.jumpForward"),
                        JumpDirection::Backward => kb.matches(event, "tui.editor.jumpBackward"),
                    };
                    if cancels_direction {
                        self.jump_mode = None;
                        return true;
                    }
                    drop(kb);
                    // Printable (with only Shift) is the jump target.
                    if let KeyCode::Char(c) = key.code
                        && (mods - KeyModifiers::SHIFT).is_empty()
                    {
                        self.jump_to_char(c, direction);
                        self.jump_mode = None;
                        return true;
                    }
                    // Anything else cancels and falls through to normal
                    // handling so the user's key (arrow, etc.) still
                    // works.
                    self.jump_mode = None;
                }

                // Autocomplete popup intercept. When a popup is open,
                // navigation/confirm/cancel/Tab have popup-specific
                // behavior; printable characters fall through so they
                // edit the buffer (and the editing path will re-run
                // suggestions via `maybe_update_autocomplete_after_edit`).
                if self.autocomplete_state.is_some() {
                    let kb = keybindings::get();
                    if kb.matches(event, "tui.select.cancel") {
                        drop(kb);
                        self.cancel_autocomplete();
                        return true;
                    }
                    if (kb.matches(event, "tui.select.up") || kb.matches(event, "tui.select.down"))
                        && self.autocomplete_list.is_some()
                    {
                        drop(kb);
                        if let Some(list) = self.autocomplete_list.as_mut() {
                            list.handle_input(event);
                        }
                        return true;
                    }
                    if kb.matches(event, "tui.input.tab") {
                        drop(kb);
                        // Tab with an open popup accepts the current
                        // selection (does not re-query as a force
                        // request). If no list, treat as cancel.
                        let selected = self
                            .autocomplete_list
                            .as_ref()
                            .and_then(|l| l.selected_item().cloned());
                        if let Some(selected) = selected {
                            self.apply_autocomplete_item(AutocompleteItem {
                                value: selected.value,
                                label: selected.label,
                                description: selected.description,
                            });
                        } else {
                            self.cancel_autocomplete();
                        }
                        return true;
                    }
                    if kb.matches(event, "tui.select.confirm") {
                        drop(kb);
                        match self.accept_autocomplete_on_enter() {
                            EnterOutcome::Consumed => return true,
                            EnterOutcome::FallThroughToSubmit => {
                                // Completion was applied for a
                                // slash-command name; keep running
                                // so the outer dispatcher's submit
                                // branch fires on this same Enter.
                            }
                        }
                    }
                    // Fall through to normal key handling. If an
                    // editing path runs, its
                    // `maybe_update_autocomplete_after_edit` call at
                    // the tail will refresh or close the popup.
                }

                let kb = keybindings::get();

                // Ctrl+C — pass through. Default binding is
                // `tui.input.copy`. Returning `false` is the editor's
                // "I didn't act on this" signal; an input listener
                // wired by the application gets a chance to handle it
                // before us, and the parent's normal-flow code can
                // observe nothing happened.
                if kb.matches(event, "tui.input.copy") {
                    return false;
                }

                // Tab without an open popup: force-request autocomplete.
                if kb.matches(event, "tui.input.tab") {
                    drop(kb);
                    self.trigger_force_autocomplete();
                    return true;
                }

                // Character-jump mode triggers.
                if kb.matches(event, "tui.editor.jumpForward") {
                    self.jump_mode = Some(JumpDirection::Forward);
                    return true;
                }
                if kb.matches(event, "tui.editor.jumpBackward") {
                    self.jump_mode = Some(JumpDirection::Backward);
                    return true;
                }

                // Undo.
                if kb.matches(event, "tui.editor.undo") {
                    drop(kb);
                    self.restore_undo();
                    return true;
                }

                // Newline: the registry-bound `tui.input.newLine`
                // (Shift+Enter by default) or the byte-form fallbacks
                // [`is_newline_event`] catches (Alt+Enter, raw LF,
                // Ctrl+J — see pi-tui's `editor.ts:716-732` newline
                // branch). Checked before submit so Shift+Enter
                // inside a submit-enabled editor produces a newline
                // instead of a submit.
                //
                // The byte-form fallback fires regardless of
                // `disable_submit` — pi-tui treats `\x1b\r`, raw LF,
                // and `\x1b[13;2~` as newline always. Pi parity (H2):
                // there is no "submit-key becomes newline under
                // `disable_submit`" arm here. Pi-tui's submit branch
                // (`editor.ts:735-749`) handles that case by silently
                // returning, not by routing the key into the newline
                // body. The `disable_submit` short-circuit lives in
                // the submit branch below, mirroring pi exactly.
                let is_newline = kb.matches(event, "tui.input.newLine") || is_newline_event(event);
                if is_newline {
                    // Inverse backslash+Enter workaround for the "swap
                    // config" (user has bound `shift+enter` to submit
                    // and `enter` to newLine): typing `\<Enter>` strips
                    // the backslash and submits, mirroring the standard
                    // workaround in the opposite direction.
                    if self.should_submit_on_backslash_enter(event, &kb) {
                        drop(kb);
                        let line = &mut self.lines[self.cursor_line];
                        line.remove(self.cursor_col - 1);
                        self.cursor_col -= 1;
                        self.submit_value();
                        return true;
                    }
                    drop(kb);
                    self.save_undo();
                    self.insert_newline_internal();
                    self.reset_sticky_state();
                    self.last_action = LastAction::None;
                    self.fire_change();
                    return true;
                }

                // Submit (Enter without modifiers, by default).
                //
                // Pi parity: when `disable_submit` is true, the submit
                // key is silently consumed (returns from the handler
                // without doing anything). See `editor.ts:735-736` —
                // pi tests `disableSubmit` *inside* the submit branch
                // and returns early, so the key is consumed but not
                // forwarded to any other handler. Returning `true`
                // here is the Rust-port equivalent of pi's bare
                // `return;` (consumed, no fall-through).
                if kb.matches(event, "tui.input.submit") {
                    if self.disable_submit {
                        return true;
                    }
                    drop(kb);

                    // Backslash+Enter newline workaround.
                    //
                    // Typing `\` immediately before Enter inserts a
                    // literal newline instead of submitting. This lets
                    // users enter multi-line input in a single-line-
                    // submit editor (terminals don't distinguish Enter
                    // from Shift+Enter without Kitty keyboard
                    // protocol). The trigger requires the `\` to be
                    // the character immediately before the cursor;
                    // typing `\x` then Enter submits normally.
                    if self.cursor_preceded_by_backslash() {
                        self.save_undo();
                        let line = &mut self.lines[self.cursor_line];
                        line.remove(self.cursor_col - 1);
                        self.cursor_col -= 1;
                        self.insert_newline_internal();
                        self.reset_sticky_state();
                        self.last_action = LastAction::None;
                        self.fire_change();
                        return true;
                    }

                    self.submit_value();
                    return true;
                }

                // Vertical movement: history-aware Up/Down off the
                // visual-line map, not the logical one.
                if kb.matches(event, "tui.editor.cursorUp") {
                    drop(kb);
                    let width = self.layout_width.get();
                    let vls = self.build_visual_line_map(width);
                    let current_vl = self.find_current_visual_line(&vls);
                    let is_empty = self.lines.len() == 1 && self.lines[0].is_empty();
                    if is_empty {
                        self.history_up();
                    } else if self.history_index.is_some() && current_vl == 0 {
                        self.history_up();
                    } else if current_vl == 0 {
                        // Top visual line already — jump to start.
                        self.cursor_col = 0;
                        self.reset_sticky_state();
                    } else {
                        self.move_up();
                    }
                    self.last_action = LastAction::None;
                    return true;
                }
                if kb.matches(event, "tui.editor.cursorDown") {
                    drop(kb);
                    let width = self.layout_width.get();
                    let vls = self.build_visual_line_map(width);
                    let current_vl = self.find_current_visual_line(&vls);
                    let on_last_vl = current_vl + 1 >= vls.len();
                    if self.history_index.is_some() && on_last_vl {
                        self.history_down();
                    } else if on_last_vl {
                        // Bottom visual line already — jump to end.
                        self.cursor_col = self.current_line().len();
                        self.reset_sticky_state();
                    } else {
                        self.move_down();
                    }
                    self.last_action = LastAction::None;
                    return true;
                }

                // Page up/down — scroll by max_visible_lines and move
                // the cursor with the viewport.
                if kb.matches(event, "tui.editor.pageUp") {
                    drop(kb);
                    self.page_scroll(-1);
                    return true;
                }
                if kb.matches(event, "tui.editor.pageDown") {
                    drop(kb);
                    self.page_scroll(1);
                    return true;
                }

                // Word movement (checked before plain cursor-left/right
                // because Alt+Left and Ctrl+Left both flow through here
                // and the word-movement bindings include
                // `["alt+left", "ctrl+left"]` which would otherwise
                // shadow plain Left if checked second on a key event
                // that carries Alt or Ctrl).
                if kb.matches(event, "tui.editor.cursorWordLeft") {
                    self.move_word_left();
                    self.reset_sticky_state();
                    self.last_action = LastAction::None;
                    return true;
                }
                if kb.matches(event, "tui.editor.cursorWordRight") {
                    self.move_word_right();
                    self.reset_sticky_state();
                    self.last_action = LastAction::None;
                    return true;
                }

                // Plain horizontal movement.
                if kb.matches(event, "tui.editor.cursorLeft") {
                    self.move_left();
                    self.last_action = LastAction::None;
                    return true;
                }
                if kb.matches(event, "tui.editor.cursorRight") {
                    self.move_right();
                    self.last_action = LastAction::None;
                    return true;
                }
                if kb.matches(event, "tui.editor.cursorLineStart") {
                    self.cursor_col = 0;
                    self.reset_sticky_state();
                    self.last_action = LastAction::None;
                    return true;
                }
                if kb.matches(event, "tui.editor.cursorLineEnd") {
                    self.cursor_col = self.current_line().len();
                    self.reset_sticky_state();
                    self.last_action = LastAction::None;
                    return true;
                }

                // Deletion. Word-level checked before char-level for
                // the same reason as word-vs-char movement above.
                if kb.matches(event, "tui.editor.deleteWordBackward") {
                    drop(kb);
                    self.kill_word_backward();
                    return true;
                }
                if kb.matches(event, "tui.editor.deleteWordForward") {
                    drop(kb);
                    self.kill_word_forward();
                    return true;
                }
                if kb.matches(event, "tui.editor.deleteCharBackward")
                    || key_id_matches(event, "shift+backspace")
                {
                    drop(kb);
                    self.backspace();
                    return true;
                }
                if kb.matches(event, "tui.editor.deleteCharForward")
                    || key_id_matches(event, "shift+delete")
                {
                    drop(kb);
                    self.delete_forward();
                    return true;
                }
                if kb.matches(event, "tui.editor.deleteToLineStart") {
                    drop(kb);
                    self.kill_to_start();
                    return true;
                }
                if kb.matches(event, "tui.editor.deleteToLineEnd") {
                    drop(kb);
                    self.kill_to_end();
                    return true;
                }

                // Yank.
                if kb.matches(event, "tui.editor.yank") {
                    drop(kb);
                    self.yank();
                    return true;
                }
                if kb.matches(event, "tui.editor.yankPop") {
                    drop(kb);
                    self.yank_pop();
                    return true;
                }

                // Drop the read guard before falling through to
                // character insertion: that path doesn't consult the
                // registry, and holding the guard across the rest of
                // the handler is unnecessary lock pressure.
                drop(kb);

                // Character insertion.
                //
                // Only insert printable characters that have no modifiers
                // other than Shift. Ctrl/Alt/Super/Hyper/Meta combos are
                // bindings, not character input — e.g. Kitty's CSI-u
                // encoding can deliver things like Super+c as
                // `KeyCode::Char('c')` plus `SUPER`, and we must ignore
                // those rather than inserting a literal `c`.
                if let KeyCode::Char(c) = key.code
                    && (mods - KeyModifiers::SHIFT).is_empty()
                {
                    self.insert_char(c);
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    fn invalidate(&mut self) {
        // No render cache to invalidate currently.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Identity theme for in-module tests — every closure passes its
    /// input through verbatim. Mirrors
    /// `tests/support/themes.rs::identity_editor_theme`.
    fn identity_theme() -> EditorTheme {
        EditorTheme {
            border_color: Arc::new(|s| s.to_string()),
            select_list: SelectListTheme {
                selected_prefix: Arc::new(|s| s.to_string()),
                selected_text: Arc::new(|s| s.to_string()),
                description: Arc::new(|s| s.to_string()),
                scroll_info: Arc::new(|s| s.to_string()),
                no_match: Arc::new(|s| s.to_string()),
            },
        }
    }

    #[test]
    fn test_editor_new() {
        let editor = Editor::new(RenderHandle::detached(), identity_theme());
        assert_eq!(editor.get_text(), "");
        assert_eq!(editor.lines.len(), 1);
    }

    /// `Editor::new` (F33 follow-up) takes the theme as a required
    /// argument and applies it directly. The render path picks up the
    /// supplied `border_color` immediately — no `set_theme` call required.
    #[test]
    fn new_applies_supplied_theme_to_render_output() {
        // Sentinel theme: wrap each border segment in `<<...>>` so the
        // output contains an unmistakable marker.
        let theme = EditorTheme {
            border_color: Arc::new(|s| format!("<<{}>>", s)),
            select_list: identity_theme().select_list,
        };

        let mut editor = Editor::new(RenderHandle::detached(), theme);
        editor.set_focused(true);
        editor.set_text("hello");
        let lines = editor.render(40);

        // The first row is the top border; with our sentinel wrapper it
        // must start with `<<` and end with `>>`.
        let top = &lines[0];
        assert!(
            top.starts_with("<<") && top.ends_with(">>"),
            "new() theme border_color must be applied: got {top:?}",
        );
    }

    #[test]
    fn test_editor_set_text() {
        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.set_text("hello\nworld");
        assert_eq!(editor.get_text(), "hello\nworld");
        assert_eq!(editor.lines.len(), 2);
        assert_eq!(editor.cursor_line, 1);
    }

    #[test]
    fn test_editor_insert_char() {
        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.insert_char('h');
        editor.insert_char('i');
        assert_eq!(editor.get_text(), "hi");
    }

    #[test]
    fn test_editor_newline() {
        // Pi parity: Shift+Enter (the default `tui.input.newLine`)
        // inserts a newline. Plain Enter under `disable_submit = true`
        // is a silent no-op (matches pi's `editor.ts:735-749` submit
        // branch returning early on `disableSubmit`).
        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.set_focused(true);
        editor.disable_submit = true;
        editor.insert_char('a');
        editor.handle_input(&InputEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
        )));
        editor.insert_char('b');
        assert_eq!(editor.get_text(), "a\nb");
    }

    #[test]
    fn test_editor_backspace() {
        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.set_text("hello");
        editor.backspace();
        assert_eq!(editor.get_text(), "hell");
    }

    #[test]
    fn test_editor_backspace_at_line_start() {
        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.set_text("hello\nworld");
        // Cursor is at end of "world".
        editor.cursor_col = 0; // Move to start of line 1.
        editor.backspace();
        assert_eq!(editor.get_text(), "helloworld");
        assert_eq!(editor.lines.len(), 1);
    }

    #[test]
    fn test_editor_kill_to_end() {
        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.set_text("hello world");
        editor.cursor_col = 5;
        editor.kill_to_end();
        assert_eq!(editor.get_text(), "hello");
        assert_eq!(editor.kill_ring.peek(), Some(" world"));
    }

    #[test]
    fn test_editor_undo() {
        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.set_text("hello");
        editor.backspace();
        assert_eq!(editor.get_text(), "hell");
        editor.restore_undo();
        assert_eq!(editor.get_text(), "hello");
    }

    #[test]
    fn test_editor_render() {
        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.set_text("hello");
        editor.set_focused(true);
        let lines = editor.render(40);
        // Should have: top border + content + bottom border.
        assert!(lines.len() >= 3);
    }

    #[test]
    fn test_editor_paste() {
        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.handle_input(&InputEvent::Paste("pasted\ntext".to_string()));
        assert_eq!(editor.get_text(), "pasted\ntext");
    }

    /// Pin down the slash-command auto-trigger alphabet (F18). The
    /// original framework gates the "keep refining the popup as the
    /// user types" branch on the regex `[a-zA-Z0-9.\-_]` applied to
    /// the just-inserted character; our [`is_identifier_char`] is the
    /// byte-level equivalent. A future tweak that, say, adds `/` or
    /// drops `_` would silently change which keystrokes auto-trigger
    /// completion inside a slash or `@` context — this test fails
    /// loudly when that alphabet drifts.
    #[test]
    fn is_identifier_char_matches_pi_tui_alphabet() {
        // ASCII alphanumeric: every letter (both cases) and digit must
        // be classified as identifier.
        for c in 'a'..='z' {
            assert!(is_identifier_char(c), "lowercase `{c}` must qualify");
        }
        for c in 'A'..='Z' {
            assert!(is_identifier_char(c), "uppercase `{c}` must qualify");
        }
        for c in '0'..='9' {
            assert!(is_identifier_char(c), "digit `{c}` must qualify");
        }
        // The three non-alphanumeric extras from pi-tui's regex.
        for c in ['.', '-', '_'] {
            assert!(is_identifier_char(c), "punctuation `{c}` must qualify");
        }
        // A representative spread of characters that must *not* qualify.
        // Whitespace, the trigger characters themselves, common ASCII
        // punctuation/symbols, and non-ASCII letters (the original
        // regex is ASCII-only; any change to broaden it is a behavior
        // change worth a deliberate test update).
        for c in [
            ' ', '\t', '\n', '/', '@', ',', ';', ':', '(', ')', '[', ']', '{', '}', '+', '=', '*',
            '?', '!', '~', '#', '$', '%', '^', '&', '<', '>', '|', '\\', '"', '\'', '`', 'é', 'ü',
            'я', '中',
        ] {
            assert!(
                !is_identifier_char(c),
                "`{c}` (U+{u:04X}) must not qualify",
                u = c as u32,
            );
        }
    }

    /// Lock in the trim-start + starts-with contract on the slash-
    /// command context check (F18). pi-tui:
    /// `isSlashMenuAllowed() && textBeforeCursor.trimStart().startsWith("/")`.
    /// We mirror that byte for byte; this test exercises the leading-
    /// whitespace tolerance, the second-line gate, and the cursor-
    /// past-the-slash invariant a future refactor could regress.
    #[test]
    fn is_in_slash_command_context_matches_pi_tui_trim_start_rule() {
        // Empty buffer: not in a slash context.
        let editor = Editor::new(RenderHandle::detached(), identity_theme());
        assert!(!editor.is_in_slash_command_context());

        // Bare leading "/" with cursor right after it: in context.
        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.set_text("/");
        assert!(editor.is_in_slash_command_context());

        // "/foo " with the cursor at end-of-line: the trailing space
        // does not close the slash context (matches pi-tui — args).
        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.set_text("/foo arg");
        assert!(editor.is_in_slash_command_context());

        // Leading whitespace before the slash: still in context. The
        // `trimStart()` half of pi-tui's predicate explicitly tolerates
        // any amount of leading spaces or tabs.
        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.set_text("   /help");
        assert!(editor.is_in_slash_command_context());

        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.set_text("\t/help");
        assert!(editor.is_in_slash_command_context());

        // A word before the slash takes us out of context.
        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.set_text("foo/bar");
        assert!(!editor.is_in_slash_command_context());

        // Slash on the second line: blocked by `is_slash_menu_allowed`,
        // which restricts the popup to the first line of a message.
        let mut editor = Editor::new(RenderHandle::detached(), identity_theme());
        editor.set_text("hello\n/help");
        // Cursor lands at end of line 1 ("/help") after `set_text`.
        assert_eq!(editor.cursor_line, 1);
        assert!(!editor.is_in_slash_command_context());
    }
}
