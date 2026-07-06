//! [`TextArea`]: a multi-line, focusable text editor with word wrapping,
//! emacs-style editing, a kill ring, undo, sticky-column vertical movement,
//! prompt history, large-paste markers, and char-jump mode.
//!
//! `TextArea` is the multi-line sibling of [`TextField`](crate::vxfw::TextField).
//! Where `TextField` is a single-line gap buffer, `TextArea` holds a document as
//! one `String` per logical line and a `(line, byte col)` cursor, wraps each
//! logical line to the layout width for display, and moves the cursor
//! vertically across wrapped rows while keeping a sticky preferred column.
//!
//! # Editing engine
//!
//! The reusable, widget-free primitives live in [`crate::text`]: the
//! [`KillRing`], the [`UndoStack`], and the word-motion engine. `TextArea`
//! defaults to the three-class [`EmacsWords`] classifier, where a run of
//! punctuation is its own word, and exposes
//! [`set_word_classifier`](TextArea::set_word_classifier) to swap it.
//!
//! # Rendering
//!
//! `draw` produces a bordered [`Surface`]: a top rule, a bounded window of
//! visible wrapped rows, and a bottom rule. The border color and top-bar label
//! inlay into the top rule, and `↑ N more` / `↓ N more` indicators reflect the
//! internal scroll offset. The caret is reported through
//! [`Surface::cursor`](crate::vxfw::Surface), which the framework renders only
//! while this widget is focused.
//!
//! # Height and scroll
//!
//! `TextArea` sizes to the `DrawContext` constraints. It never reads terminal
//! rows. The visible-row cap is `min(max_visible_rows.unwrap_or(available),
//! available)` where `available` is `ctx.max.height` minus the two border rows,
//! so a flex-0 host slot grows with content up to the cap and scrolls beyond.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use unicode_segmentation::UnicodeSegmentation;

use crate::cell::{Cell, Character, Color, CursorShape, Style};
use crate::gwidth;
use crate::key::{Key, Modifiers};
use crate::text::{
    EmacsWords, KillRing, UndoStack, WordClassifier, is_punctuation_grapheme,
    is_whitespace_grapheme, skip_class, skip_separators, word_left,
};
use crate::vxfw::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSession, AutocompleteSuggestions,
    CursorState, DrawContext, Event, EventContext, SessionInvalid, Size, SuggestOpts, Surface,
    Widget,
};

/// Prefix every large-paste marker token starts with, e.g. the `[paste #` of
/// `[paste #1 +20 lines]`. Used as the cheap first check before the fuller
/// [`find_next_marker`] scan.
const PASTE_MARKER_PREFIX: &str = "[paste #";

/// Coalescing window for the implicit `@` / `#` symbol-completion triggers. A
/// burst of keystrokes inside a symbol token faster than this collapses into a
/// single provider call rather than one call per character. Tab (force) and
/// non-symbol contexts fire immediately.
const ATTACHMENT_AUTOCOMPLETE_DEBOUNCE: Duration = Duration::from_millis(20);

/// Default popup height cap, in rows, before the suggestion list scrolls.
/// Clamped to `[3, 20]` by
/// [`set_autocomplete_max_visible`](TextArea::set_autocomplete_max_visible).
const AUTOCOMPLETE_MAX_VISIBLE_DEFAULT: usize = 5;

/// Per-frame budget the UI thread spends advancing the incremental matcher of a
/// streaming autocomplete session.
///
/// The fs walk runs on a blocking task and nucleo converges over multiple
/// notify-driven frames, so a small budget keeps typing snappy without stalling
/// popup population: we do a little matching work each frame rather than one big
/// blocking pass that would delay the render after a keystroke.
const AUTOCOMPLETE_TICK_BUDGET_MS: u64 = 2;

/// Which way [`TextArea::jump_to_char`] scans for its target.
///
/// Forward lands on the first occurrence strictly after the cursor. Backward
/// lands on the last occurrence strictly before it. Both scan across logical
/// lines when the current line has no match, and both are case-sensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JumpDirection {
    Forward,
    Backward,
}

/// Finds the next paste-marker token in `line` at or after byte offset `from`,
/// validated against `pastes`. Returns the `(start, end, id)` byte span of the
/// match.
///
/// A token must be closed with `]` and match one of the two known tail shapes
/// (see [`parse_marker_tail`]). We also require the parsed id to be present in
/// `pastes`. That validation is what stops a manually typed marker-like string
/// (say `[paste #99 +5 lines]` with no matching paste) from being treated as an
/// atomic unit. Only tokens we actually created are atomic.
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
        let id_start = i + prefix.len();
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
        let Some(end_rel) = parse_marker_tail(&line[id_end..]) else {
            i += 1;
            continue;
        };
        // Validation: a marker-shaped token whose id we never issued is not
        // atomic. Skip it and keep scanning so typed text is never collapsed.
        if !pastes.contains_key(&id) {
            i += 1;
            continue;
        }
        return Some((i, id_end + end_rel, id));
    }
    None
}

/// Parses the tail of a paste marker starting right after the id digits.
///
/// Returns the byte length of the tail including its closing `]` for the three
/// accepted shapes, or `None` for anything else:
///
/// - `"]"` yields `Some(1)`
/// - `" +123 lines]"` yields `Some(12)`
/// - `" 1234 chars]"` yields `Some(12)`
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

/// Byte span of a paste marker that begins exactly at `col`, if any.
fn marker_starting_at(
    line: &str,
    col: usize,
    pastes: &HashMap<u32, String>,
) -> Option<(usize, usize)> {
    find_next_marker(line, col, pastes)
        .filter(|(s, _, _)| *s == col)
        .map(|(s, e, _)| (s, e))
}

/// Byte span of a paste marker that ends exactly at `col`, if any.
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

/// Decodes Kitty CSI-u `Ctrl+<letter>` sequences embedded in pasted text back to
/// their literal control byte.
///
/// Some terminals (notably tmux popups with `extended-keys-format=csi-u`)
/// re-encode control bytes inside a bracketed paste as `ESC [ <codepoint> ; 5 u`.
/// The classic case is `ESC [ 106 ; 5 u` (Ctrl+J, a newline) between pasted
/// lines. Without this decode the per-char control filter would strip the `ESC`
/// and leak the printable tail (`[106;5u`) into the buffer, garbling the paste.
///
/// Only `Ctrl+<ASCII letter>` codepoints (97..=122 and 65..=90) are decoded.
/// Other modifier-5 sequences are left intact so the filter handles them as
/// before.
fn decode_csi_u_ctrl_letters(text: &str) -> String {
    // Fast path: no ESC means nothing to decode, so we avoid an allocation on
    // the common case of pasting normal text.
    if !text.as_bytes().contains(&0x1b) {
        return text.to_string();
    }

    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        // Copy a whole non-ASCII codepoint through by scalar so the byte
        // scanner never splits a multi-byte UTF-8 sequence.
        if !bytes[i].is_ascii() {
            let ch = text[i..].chars().next().expect("char boundary");
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if bytes[i] != 0x1b || i + 1 >= bytes.len() || bytes[i + 1] != b'[' {
            out.push(char::from(bytes[i]));
            i += 1;
            continue;
        }
        let digits_start = i + 2;
        let mut digits_end = digits_start;
        while digits_end < bytes.len() && bytes[digits_end].is_ascii_digit() {
            digits_end += 1;
        }
        // Require at least one digit followed by `;5u`. Anything else (a
        // cursor-movement sequence, say) is left untouched.
        let tail_ok = digits_end > digits_start
            && digits_end + 3 <= bytes.len()
            && &bytes[digits_end..digits_end + 3] == b";5u";
        if !tail_ok {
            out.push(char::from(bytes[i]));
            i += 1;
            continue;
        }
        let code: u32 = text[digits_start..digits_end].parse().unwrap_or(0);
        let decoded = match code {
            97..=122 => char::from_u32(code - 96),
            65..=90 => char::from_u32(code - 64),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                i = digits_end + 3;
            }
            None => {
                // Out of range: leave the literal `ESC[<digits>;5u` bytes in
                // place so the existing filter handles them.
                out.push(char::from(bytes[i]));
                i += 1;
            }
        }
    }
    out
}

/// Whether `c`, typed inside an existing `@` / `#` symbol context, keeps the
/// popup open. Matches the character class `[A-Za-z0-9.\-_]` the trigger gating
/// uses to decide a keystroke continues a symbol token.
fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')
}

/// Whether `before` (the text on the current line up to the cursor) ends in an
/// `@`- or `#`-prefixed token at a token boundary. Equivalent to the regex
/// `(?:^|\s)[@#][^\s]*$`.
///
/// The trigger, re-trigger, and debounce predicates all route through this so
/// they agree on where a symbol token starts. Both `@` and `#` are recognized:
/// the mechanism is provider-agnostic, and a provider with no candidates for a
/// symbol simply returns nothing.
///
/// NOTE: an in-progress quoted attachment like `@"has spaces` (no closing quote
/// yet) is rejected as soon as a space lands. Closing that gap would need a
/// stateful scan that tracks the open quote, which no caller needs today.
fn ends_in_symbol_context(before: &str) -> bool {
    // The rightmost of the two symbols wins, so `@a #b` matches on the `#`,
    // matching the single-symbol regex.
    let sym_byte_idx = match (before.rfind('@'), before.rfind('#')) {
        (None, None) => return false,
        (Some(a), None) => a,
        (None, Some(h)) => h,
        (Some(a), Some(h)) => a.max(h),
    };
    let sym_char = before[sym_byte_idx..]
        .chars()
        .next()
        .expect("rfind returned a valid char boundary");
    let after_sym = &before[sym_byte_idx + sym_char.len_utf8()..];
    if after_sym.chars().any(char::is_whitespace) {
        return false;
    }
    if sym_byte_idx == 0 {
        return true;
    }
    before[..sym_byte_idx]
        .chars()
        .last()
        .is_some_and(char::is_whitespace)
}
/// Colors and styles the editor draws with: the border rules and the inline
/// autocomplete popup.
#[derive(Debug, Clone, Default)]
pub struct EditorTheme {
    /// Color of the top and bottom border rules and their inlaid text.
    pub border_color: Color,
    /// Styling for the inline autocomplete popup.
    pub popup: PopupStyle,
}

/// Styling for the inline autocomplete popup: one style for unselected rows and
/// one for the highlighted row, which the popup fills edge-to-edge as a band.
#[derive(Debug, Clone, Default)]
pub struct PopupStyle {
    /// Style for an unselected suggestion row.
    pub item: Style,
    /// Style for the highlighted suggestion row.
    pub selected: Style,
}

/// A fixed editing chord, for documentation surfaces (the help screen).
///
/// `TextArea`'s chords are hard-coded, not rebindable, so [`TextArea::bindings`]
/// returns a static slice of these as the single source of truth shared by the
/// event handler and the docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChordDoc {
    /// Human-readable key label, e.g. `"Ctrl-A / Home"`.
    pub keys: &'static str,
    /// What the chord does.
    pub description: &'static str,
    /// Display group the help screen buckets the chord under.
    pub group: &'static str,
}

/// Snapshot of the document and cursor for one undo step.
#[derive(Debug, Clone)]
struct EditorSnapshot {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
}

/// The last mutating action, used to coalesce undo snapshots, accumulate kills,
/// and gate yank-pop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    None,
    Kill,
    Yank,
    TypeWord,
}

/// Why an autocomplete popup is currently showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutocompleteMode {
    /// Triggered implicitly by typing `@` / `#`. Closes automatically when the
    /// typed prefix stops matching or the cursor leaves the trigger context.
    Regular,
    /// Triggered explicitly by Tab. Stays open across typing so the user can
    /// narrow a result set, and applies the highlighted item on a second Tab.
    Force,
}

/// Captured buffer state used to detect stale autocomplete deliveries. Any
/// field changing between dispatch and delivery means the user has moved on and
/// the result no longer applies to the current cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AutocompleteSnapshot {
    text: String,
    cursor_line: usize,
    cursor_col: usize,
}

/// A message an autocomplete worker sends back to the widget.
///
/// Opaque to the host: the host drains one from the receiver handed out by
/// [`TextArea::take_autocomplete_rx`] and passes it straight to
/// [`TextArea::apply_autocomplete_delivery`], which applies the staleness
/// guards. Two kinds ride the same channel so the host needs a single arm: a
/// completed one-shot query, and a "session progressed" wake from a streaming
/// session's worker threads.
pub struct AutocompleteDelivery {
    kind: DeliveryKind,
}

enum DeliveryKind {
    /// A completed one-shot [`AutocompleteProvider::get_suggestions`] call.
    Query {
        /// Identifier of the request that produced this delivery. Compared
        /// against the widget's current id before any state changes. A stale
        /// delivery is dropped silently.
        request_id: u64,
        /// Buffer state captured when the request was dispatched. Compared
        /// against the current buffer before the list is applied.
        snapshot: AutocompleteSnapshot,
        /// The suggestions, or `None` when the provider found nothing.
        suggestions: Option<AutocompleteSuggestions>,
        /// The popup mode the request was dispatched in, preserved across the
        /// round-trip so a late delivery still opens the popup in the right
        /// mode.
        mode: AutocompleteMode,
        /// Whether to auto-apply a single result (the Tab / force path).
        auto_apply_single: bool,
    },
    /// A streaming session's worker pushed new matches. The widget ticks the
    /// session and rebuilds the popup.
    SessionProgressed,
}

impl AutocompleteDelivery {
    fn query(
        request_id: u64,
        snapshot: AutocompleteSnapshot,
        suggestions: Option<AutocompleteSuggestions>,
        mode: AutocompleteMode,
        auto_apply_single: bool,
    ) -> Self {
        Self {
            kind: DeliveryKind::Query {
                request_id,
                snapshot,
                suggestions,
                mode,
                auto_apply_single,
            },
        }
    }

    fn session_progressed() -> Self {
        Self {
            kind: DeliveryKind::SessionProgressed,
        }
    }
}

/// A single visual (screen) line produced by wrapping one logical document line
/// at the current layout width.
///
/// Byte-offset based: `start_col` is the index into `lines[logical_line]` where
/// the visible span begins, and `length` is the byte length of that span.
/// Concatenating the spans of a logical line reconstructs it exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisualLine {
    logical_line: usize,
    start_col: usize,
    length: usize,
}

/// The visual-line map cached from the last build, keyed by the width it was
/// built at.
///
/// Valid only while the document is unchanged. Any edit to `lines` drops it via
/// [`TextArea::invalidate_visual_line_cache`], and a width change is detected by
/// [`TextArea::visual_line_map`] which rebuilds.
struct VisualLineCache {
    width: usize,
    lines: Vec<VisualLine>,
}

/// One atomic display segment of a logical line.
///
/// A segment is one grapheme cluster or one whole paste-marker token. The
/// wrapper and the vertical-move snap treat a segment as indivisible, which
/// keeps a multi-byte grapheme (a ZWJ emoji, base plus combining mark) whole
/// across wrap boundaries, keeps a paste marker from being split in half or
/// entered mid-token by the cursor, and stops the cursor landing in the middle
/// of either.
#[derive(Debug, Clone, Copy)]
struct AtomicSegment<'a> {
    text: &'a str,
    start_index: usize,
}

/// A multi-line text editor widget. See the module docs for the model.
pub struct TextArea {
    // -- Document --
    /// One string per logical line. Invariant: never empty. An empty document
    /// is `vec![String::new()]`.
    lines: Vec<String>,
    /// Cursor line index.
    cursor_line: usize,
    /// Cursor byte offset within `lines[cursor_line]`.
    cursor_col: usize,

    // -- Vertical-movement state --
    /// First visible visual row. Persisted across draws as the scroll hint.
    scroll_offset: usize,
    /// Sticky column (in bytes into a visual line) for vertical movement.
    preferred_visual_col: Option<usize>,
    /// Byte offset the cursor would have occupied on its source line had the
    /// last vertical move not snapped it onto an atomic segment. The next
    /// vertical move reads this so the sticky math reflects the user's intent
    /// rather than the post-snap column. Cleared by any edit or horizontal move
    /// through [`TextArea::reset_sticky_state`].
    snapped_from_cursor_col: Option<usize>,

    // -- Editing state --
    kill_ring: KillRing,
    undo_stack: UndoStack<EditorSnapshot>,
    last_action: LastAction,
    word_classifier: Box<dyn WordClassifier>,

    // -- Large-paste markers --
    //
    // A large paste is collapsed to a short marker token in `lines` while its
    // literal content is stashed here, keyed by the id embedded in the token.
    // `expanded_text` splices the content back in.
    //
    // NOTE: The map is deliberately not part of the undo snapshot. Undo restores
    // the marker *text* in `lines`, and the map persists, so an undone-then-
    // redone marker still resolves. The counter only ever grows, so a fresh
    // paste never reuses a live id. Stale entries left after `set_text` or
    // history navigation are harmless: markers validate against the map, and an
    // id with no matching token is simply not treated as atomic.
    pastes: HashMap<u32, String>,
    /// Monotonic id source for paste markers. Never rewinds except on submit,
    /// which clears the whole paste state.
    paste_counter: u32,

    // -- Char-jump mode --
    /// When set, the next printable key is a jump target rather than input.
    /// See [`TextArea::jump_to_char`]. The mode is invisible: no prompt or UI
    /// while it is active.
    jump_mode: Option<JumpDirection>,

    // -- History --
    history: Vec<String>,
    history_index: Option<usize>,

    // -- Autocomplete --
    //
    // The widget owns its autocomplete state machine: the popup pops up inside
    // this widget's own surface, and keystrokes route between the document
    // (typing, cursor) and the popup (navigation, accept) inline, so splitting
    // it into an overlay would push routing into every caller.
    //
    // The async pipeline lives here too, but the host owns the wake: the widget
    // spawns workers that deliver results down `autocomplete_tx`, and the host
    // drains the paired receiver (see `take_autocomplete_rx`) from its own
    // `select!`. The widget never runs a reader thread of its own.
    /// The installed provider, shared with every spawned worker.
    autocomplete_provider: Option<Arc<dyn AutocompleteProvider>>,
    /// `None` = popup closed. `Some(mode)` = popup open in that mode.
    autocomplete_state: Option<AutocompleteMode>,
    /// The suggestions currently shown. Empty while a Force popup has no
    /// matches (still open so the user can narrow).
    autocomplete_items: Vec<AutocompleteItem>,
    /// Index of the highlighted row in `autocomplete_items`.
    autocomplete_selected: usize,
    /// Substring the suggestions match against. The chosen item's `value`
    /// replaces exactly this many bytes before the cursor when applied.
    autocomplete_prefix: String,
    /// Popup height cap, in rows, clamped to `[3, 20]`.
    autocomplete_max_visible: usize,
    /// A live streaming session, when a provider opted into
    /// [`AutocompleteProvider::try_start_session`] for the current context.
    /// Mutually exclusive with a pending one-shot request: the widget prefers
    /// the session and only dispatches when `try_start_session` returned `None`.
    autocomplete_session: Option<Box<dyn AutocompleteSession>>,
    /// Monotonically-increasing token bumped on every request start. A delivery
    /// whose id no longer matches is stale and dropped.
    autocomplete_request_id: u64,
    /// Cancellation token for the pending request, tripped before a new request
    /// spawns and when the popup is dismissed.
    autocomplete_cancel: Option<CancellationToken>,
    /// Join handle for the pending worker. Held only so tests can await
    /// completion deterministically; production relies on the cancel token and
    /// the delivery channel and never joins.
    autocomplete_task: Option<JoinHandle<()>>,
    /// Sender half of the delivery channel, cloned into every worker.
    autocomplete_tx: mpsc::UnboundedSender<AutocompleteDelivery>,
    /// Receiver half, until the host takes it via `take_autocomplete_rx`. While
    /// the widget still holds it, `pump_autocomplete` drains it in-place so a
    /// host that prefers a single pump entry point still sees deliveries.
    autocomplete_rx: Option<mpsc::UnboundedReceiver<AutocompleteDelivery>>,

    // -- Presentation --
    theme: EditorTheme,
    padding_x: usize,
    top_bar_label: Option<String>,
    /// Explicit cap on visible rows. `None` uses the constraint-derived cap.
    max_visible_rows: Option<usize>,

    // -- Layout state cached from the last draw --
    //
    // Navigation happens in `handle_event`, where there is no `DrawContext`, so
    // draw stashes the width it laid out at and the width-measurement method
    // here for the visual-line map to reuse. Defaults keep navigation before
    // the first draw sane.
    layout_width: usize,
    width_method: gwidth::Method,
    /// Visible-row count from the last draw. Used as the page size for
    /// PageUp/PageDown.
    last_visible_rows: usize,
    /// Cached visual-line map, keyed by the width it was built at. See
    /// [`TextArea::visual_line_map`]. Dropped by
    /// [`TextArea::invalidate_visual_line_cache`] on every document edit, so it
    /// is never served stale.
    visual_line_cache: Option<VisualLineCache>,

    // -- Submission --
    /// If false, Enter is silently consumed instead of submitting.
    submit_enabled: bool,
    /// Set on submit, polled by the host as an alternative to `on_submit`.
    submitted_text: Option<String>,
    /// Text as of the last `on_change` check, used to suppress no-op fires.
    previous_val: String,

    /// Fires during event handling when an edit changes the text.
    pub on_change: Option<Box<dyn FnMut(&mut EventContext, &str)>>,
    /// Fires on submit with the (trimmed) contents while the editor clears.
    pub on_submit: Option<Box<dyn FnMut(&mut EventContext, &str)>>,
    /// Fires when the user types `/` at an empty start-of-message position. The
    /// `/` keystroke is swallowed, not inserted, when this is set.
    pub on_palette_trigger: Option<Box<dyn FnMut(&mut EventContext)>>,
}

impl TextArea {
    /// Maximum history entries retained. Once reached, each new entry drops the
    /// oldest.
    pub const HISTORY_LIMIT: usize = 100;

    /// A new empty editor behind an `Rc<RefCell<..>>`, matching the widget
    /// convention.
    pub fn new() -> Rc<RefCell<TextArea>> {
        Rc::new(RefCell::new(Self::new_state()))
    }

    /// Builds the bare widget state. Kept separate from [`TextArea::new`] so
    /// in-crate tests can drive a plain `TextArea` without the `Rc<RefCell<..>>`
    /// wrapper.
    fn new_state() -> TextArea {
        let (autocomplete_tx, autocomplete_rx) = mpsc::unbounded_channel();
        TextArea {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            scroll_offset: 0,
            preferred_visual_col: None,
            snapped_from_cursor_col: None,
            kill_ring: KillRing::new(),
            undo_stack: UndoStack::new(),
            last_action: LastAction::None,
            word_classifier: Box::new(EmacsWords),
            pastes: HashMap::new(),
            paste_counter: 0,
            jump_mode: None,
            history: Vec::new(),
            history_index: None,
            autocomplete_provider: None,
            autocomplete_state: None,
            autocomplete_items: Vec::new(),
            autocomplete_selected: 0,
            autocomplete_prefix: String::new(),
            autocomplete_max_visible: AUTOCOMPLETE_MAX_VISIBLE_DEFAULT,
            autocomplete_session: None,
            autocomplete_request_id: 0,
            autocomplete_cancel: None,
            autocomplete_task: None,
            autocomplete_tx,
            autocomplete_rx: Some(autocomplete_rx),
            theme: EditorTheme::default(),
            padding_x: 0,
            top_bar_label: None,
            max_visible_rows: None,
            layout_width: 80,
            width_method: gwidth::Method::Unicode,
            last_visible_rows: 10,
            visual_line_cache: None,
            submit_enabled: true,
            submitted_text: None,
            previous_val: String::new(),
            on_change: None,
            on_submit: None,
            on_palette_trigger: None,
        }
    }

    /// The fixed editing chords, the single source of truth for both the
    /// handler and the help screen.
    pub fn bindings() -> &'static [ChordDoc] {
        &BINDINGS
    }

    // -- Content --

    /// The full text, logical lines joined by `\n`.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// The full text with every paste-marker token replaced by the literal
    /// content it stands in for.
    ///
    /// This is the value submitted to `on_submit` and returned by
    /// [`TextArea::take_submitted`], so a consumer sees the real pasted bytes,
    /// not the placeholder. Callers wanting the displayed text (markers and all)
    /// use [`TextArea::text`].
    pub fn expanded_text(&self) -> String {
        // Fast path: with no live pastes the document is already literal.
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

    /// Appends `line` to `out`, splicing each paste-marker token back to its
    /// stored content. A token whose id is missing from the map falls back to
    /// the literal token text, so an expansion never drops characters.
    fn append_with_markers_expanded(&self, out: &mut String, line: &str) {
        let mut i = 0;
        while let Some((s, e, id)) = find_next_marker(line, i, &self.pastes) {
            out.push_str(&line[i..s]);
            match self.pastes.get(&id) {
                Some(content) => out.push_str(content),
                None => out.push_str(&line[s..e]),
            }
            i = e;
        }
        out.push_str(&line[i..]);
    }

    /// The cursor as `(line, col)`, where `col` counts Unicode scalar values
    /// from the start of the line, not bytes. For pure-ASCII input `col` equals
    /// the byte offset.
    pub fn cursor(&self) -> (usize, usize) {
        let col = self.current_line()[..self.cursor_col].chars().count();
        (self.cursor_line, col)
    }

    /// Replaces all text. The cursor moves to the end.
    ///
    /// Input is normalized: `\r\n` and lone `\r` collapse to `\n`, and `\t`
    /// expands to four spaces. `on_change` does not fire here, only on
    /// interactive edits (see [`TextArea::handle_event`]).
    pub fn set_text(&mut self, text: &str) {
        self.invalidate_visual_line_cache();
        self.save_undo();
        let normalized = Self::normalize_text(text);
        self.lines = if normalized.is_empty() {
            vec![String::new()]
        } else {
            normalized.split('\n').map(str::to_string).collect()
        };
        self.cursor_line = self.lines.len() - 1;
        self.cursor_col = self.lines[self.cursor_line].len();
        self.reset_sticky_state();
        // A wholesale replacement is a hard break: no kill accumulation, yank
        // cycle, or word-repeat coalescing should carry across it.
        self.last_action = LastAction::None;
        // Exit history browsing, dropping the draft `history_up` parked at the
        // tail if we were mid-browse.
        if self.history_index.is_some() {
            self.history.pop();
            self.history_index = None;
        }
    }

    /// Inserts `text` at the cursor as one undo unit.
    ///
    /// Input is normalized like [`TextArea::set_text`]. Other control
    /// characters are stripped so a pasted `\0` never lands in the document.
    pub fn insert_at_cursor(&mut self, text: &str) {
        self.invalidate_visual_line_cache();
        self.save_undo();
        self.cancel_autocomplete();
        self.history_index = None;
        let normalized = Self::normalize_text(text);
        for ch in normalized.chars() {
            if ch == '\n' {
                self.insert_newline_internal();
            } else if !ch.is_control() {
                self.lines[self.cursor_line].insert(self.cursor_col, ch);
                self.cursor_col += ch.len_utf8();
            }
        }
        self.reset_sticky_state();
        self.last_action = LastAction::None;
    }

    /// Handles a bracketed paste: normalize, strip control bytes, apply the
    /// path-prefix separator, then either insert literally or collapse to a
    /// large-paste marker.
    ///
    /// One undo unit. Fires no callback here. The `Event::Paste` handler follows
    /// this with `check_changed`.
    fn handle_paste(&mut self, text: &str) {
        self.invalidate_visual_line_cache();
        self.save_undo();
        // A paste is a bulk insert, not a keystroke, so it closes any popup
        // rather than re-querying per pasted character.
        self.cancel_autocomplete();
        // Undo re-encoded control bytes some terminals inject into a bracketed
        // paste before the per-char filter below would strip the ESC and leak
        // the printable tail.
        let decoded = decode_csi_u_ctrl_letters(text);
        let normalized = Self::normalize_text(&decoded);
        // Drop control characters. The newline and the four-space tab expansion
        // are already in the normalized text.
        let mut filtered = String::with_capacity(normalized.len());
        for ch in normalized.chars() {
            match ch {
                '\n' => filtered.push('\n'),
                c if c.is_control() => {}
                c => filtered.push(c),
            }
        }

        // Path-prefix safety: when the paste begins with `/`, `~`, or `.` and
        // the cursor sits right after a word-class grapheme, insert a separating
        // space first so the paste does not read like one token with the
        // preceding word (`cd` + `/etc/hosts` -> `cd /etc/hosts`). The space
        // goes into the line buffer, not the stored paste content, so
        // `expanded_text` reflects it for both inline and marker pastes.
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

        // Large-paste threshold: more than 10 lines or more than 1000 characters
        // collapses to a marker so a long file or a screenful of logs stays
        // legible in the editor.
        let line_count = filtered.matches('\n').count() + 1;
        let char_count = filtered.len();
        let use_marker = line_count > 10 || char_count > 1000;

        if use_marker {
            self.paste_counter += 1;
            let id = self.paste_counter;
            self.pastes.insert(id, filtered.clone());
            let marker = if line_count > 10 {
                format!("[paste #{id} +{line_count} lines]")
            } else {
                format!("[paste #{id} {char_count} chars]")
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
    }

    /// Clears the document and undo history, leaving one empty line.
    ///
    /// The paste map is intentionally left alone (see the field docs): a stale
    /// entry is harmless because markers validate against the map.
    pub fn clear(&mut self) {
        self.invalidate_visual_line_cache();
        self.lines = vec![String::new()];
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.reset_sticky_state();
        self.undo_stack.clear();
        self.last_action = LastAction::None;
        self.history_index = None;
    }

    /// Takes the submitted text, if any, clearing it. Returns `Some` at most
    /// once per submit. An alternative to the `on_submit` callback.
    pub fn take_submitted(&mut self) -> Option<String> {
        self.submitted_text.take()
    }

    // -- Submission config --

    /// Enables or disables submit. When disabled, Enter is silently consumed.
    pub fn set_submit_enabled(&mut self, enabled: bool) {
        self.submit_enabled = enabled;
    }

    // -- History --

    /// Adds `text` to the history for up/down navigation.
    ///
    /// Whitespace-only strings and a duplicate of the most recent entry are
    /// ignored. The ring is capped at [`TextArea::HISTORY_LIMIT`]; once full,
    /// the oldest entry is dropped.
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

    /// Splices older entries in beneath whatever the ring already holds.
    ///
    /// `entries` are oldest-first. They land before any prompts already in the
    /// ring, so submissions made this session stay the most-recent ones an Up
    /// press reaches first. Safe to call mid-browse: a browse cursor (and the
    /// draft parked at the tail) index into the ring, so we shift
    /// `history_index` by the net change at the front and only ever drop from
    /// the front to keep the draft at the tail.
    pub fn seed_history(&mut self, entries: &[String]) {
        if entries.is_empty() {
            return;
        }
        let added = entries.len();
        let mut seeded = Vec::with_capacity(added + self.history.len());
        seeded.extend(entries.iter().cloned());
        seeded.append(&mut self.history);
        self.history = seeded;
        if let Some(idx) = self.history_index.as_mut() {
            *idx += added;
        }
        if self.history.len() > Self::HISTORY_LIMIT {
            let overflow = self.history.len() - Self::HISTORY_LIMIT;
            self.history.drain(..overflow);
            if let Some(idx) = self.history_index.as_mut() {
                *idx = idx.saturating_sub(overflow);
            }
        }
    }

    // -- Presentation --

    /// Replaces the theme.
    pub fn set_theme(&mut self, theme: EditorTheme) {
        self.theme = theme;
    }

    /// Sets the border color (a thinking/bash-mode tint), independent of the
    /// rest of the theme.
    pub fn set_border_color(&mut self, color: Color) {
        self.theme.border_color = color;
    }

    /// Sets horizontal padding, in columns, on each side.
    pub fn set_padding_x(&mut self, cols: usize) {
        self.padding_x = cols;
    }

    /// Inlays a short label into the top border. `None` leaves a plain rule.
    pub fn set_top_bar_label(&mut self, label: Option<String>) {
        self.top_bar_label = label;
    }

    /// Caps the visible rows revealed before scrolling. `None` uses the
    /// constraint-derived cap.
    pub fn set_max_visible_rows(&mut self, max: Option<usize>) {
        self.max_visible_rows = max;
    }

    /// Swaps the word classifier that drives word motions and word kills. The
    /// default is [`EmacsWords`].
    pub fn set_word_classifier(&mut self, classifier: Box<dyn WordClassifier>) {
        self.word_classifier = classifier;
    }

    // -- Autocomplete: public API --

    /// Installs the completion provider. Any open popup and pending request are
    /// cancelled first, so switching providers never applies a stale result.
    ///
    /// Held as an `Arc` because the widget hands a cloned reference to every
    /// spawned worker task. `Send + Sync` is what makes that share safe.
    pub fn set_autocomplete_provider(&mut self, provider: Arc<dyn AutocompleteProvider>) {
        self.cancel_autocomplete();
        self.autocomplete_provider = Some(provider);
    }

    /// Caps the popup height, in rows, before the list scrolls. Clamped to
    /// `[3, 20]`.
    pub fn set_autocomplete_max_visible(&mut self, max: usize) {
        self.autocomplete_max_visible = max.clamp(3, 20);
    }

    /// Whether the autocomplete popup is currently open.
    pub fn is_showing_autocomplete(&self) -> bool {
        self.autocomplete_state.is_some()
    }

    /// Takes the delivery receiver so the host can `select!` on it. Returns
    /// `None` on a second call: the receiver is handed out once, at wiring.
    ///
    /// The host adds one arm that, on each delivery, calls
    /// [`apply_autocomplete_delivery`](Self::apply_autocomplete_delivery) and
    /// requests a redraw. This keeps the widget a library: it owns the pipeline
    /// but never runs the `select!`. A host that would rather poll can skip
    /// this and call [`pump_autocomplete`](Self::pump_autocomplete), which
    /// drains the receiver in place while the widget still holds it.
    pub fn take_autocomplete_rx(
        &mut self,
    ) -> Option<mpsc::UnboundedReceiver<AutocompleteDelivery>> {
        self.autocomplete_rx.take()
    }

    /// Advances autocomplete without a specific delivery in hand: ticks any
    /// streaming session, and, while the widget still owns the receiver, drains
    /// pending deliveries and applies the freshest one.
    ///
    /// A host that took the receiver via
    /// [`take_autocomplete_rx`](Self::take_autocomplete_rx) feeds deliveries
    /// through [`apply_autocomplete_delivery`](Self::apply_autocomplete_delivery)
    /// instead; for that host this only ticks the session. Either way the host
    /// calls it from its own loop, not the widget: `draw` never drains, so the
    /// host's `select!` arm stays the single drain point.
    pub fn pump_autocomplete(&mut self) {
        self.pump_autocomplete_session();

        // If the host took the receiver, there is nothing to drain here.
        // Otherwise keep only the freshest matching query and apply it, exactly
        // as the host's arm would. We read the id into a local first so the
        // receiver borrow does not overlap the later `&mut self` apply.
        let current_id = self.autocomplete_request_id;
        let mut latest: Option<AutocompleteDelivery> = None;
        if let Some(rx) = self.autocomplete_rx.as_mut() {
            while let Ok(delivery) = rx.try_recv() {
                match &delivery.kind {
                    DeliveryKind::Query { request_id, .. } if *request_id == current_id => {
                        latest = Some(delivery);
                    }
                    // A stale query or an already-pumped session wake.
                    DeliveryKind::Query { .. } | DeliveryKind::SessionProgressed => {}
                }
            }
        }
        if let Some(delivery) = latest {
            self.apply_autocomplete_delivery(delivery);
        }
    }

    /// Applies one delivery after the staleness guards pass. The host calls
    /// this from its `select!` arm; [`pump_autocomplete`](Self::pump_autocomplete)
    /// calls it for hosts that poll instead.
    ///
    /// # Async race safety
    ///
    /// A fast query landing between two keystrokes must never apply to a buffer
    /// the user has already moved past. Two guards enforce that: the request id
    /// (a superseded request's delivery is dropped) and the buffer snapshot (if
    /// the text or cursor moved since dispatch, the delivery is dropped even
    /// when the id still matches). A session that took over since dispatch also
    /// wins: its list must not be clobbered by an older one-shot result.
    pub fn apply_autocomplete_delivery(&mut self, delivery: AutocompleteDelivery) {
        match delivery.kind {
            DeliveryKind::SessionProgressed => self.pump_autocomplete_session(),
            DeliveryKind::Query {
                request_id,
                snapshot,
                suggestions,
                mode,
                auto_apply_single,
            } => {
                if request_id != self.autocomplete_request_id {
                    return;
                }
                if self.autocomplete_session.is_some() {
                    return;
                }
                let current = AutocompleteSnapshot {
                    text: self.text(),
                    cursor_line: self.cursor_line,
                    cursor_col: self.cursor_col,
                };
                if current != snapshot {
                    return;
                }
                self.apply_suggestions(suggestions, mode, auto_apply_single);
            }
        }
    }

    // -- Private: text normalization and undo --

    /// Normalizes text for storage: `\r\n` and lone `\r` collapse to `\n`, and
    /// every `\t` expands to four spaces. The internal model never holds an
    /// embedded `\r` or `\t`.
    fn normalize_text(text: &str) -> String {
        text.replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ")
    }

    fn save_undo(&mut self) {
        self.undo_stack.push(EditorSnapshot {
            lines: self.lines.clone(),
            cursor_line: self.cursor_line,
            cursor_col: self.cursor_col,
        });
    }

    fn restore_undo(&mut self) {
        if let Some(snapshot) = self.undo_stack.pop() {
            // Exit history browsing, dropping the draft parked at the tail if we
            // were mid-browse so the ring returns to its pre-browse shape.
            if self.history_index.is_some() {
                self.history.pop();
                self.history_index = None;
            }
            self.lines = snapshot.lines;
            self.invalidate_visual_line_cache();
            self.cursor_line = snapshot.cursor_line;
            self.cursor_col = snapshot.cursor_col;
            self.reset_sticky_state();
            self.last_action = LastAction::None;
        }
    }

    fn current_line(&self) -> &str {
        &self.lines[self.cursor_line]
    }

    fn is_empty_doc(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// Grapheme-cluster byte boundaries for the current line, including `0` and
    /// the line length.
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

    /// Display width of `s` in cells, using the last draw's width method.
    fn measure(&self, s: &str) -> usize {
        usize::from(gwidth::gwidth(s, self.width_method))
    }

    // -- Horizontal movement --

    /// Moves the cursor left by one grapheme, wrapping to the end of the
    /// previous line at column zero. Any horizontal move clears the sticky
    /// column.
    fn move_left(&mut self) {
        self.reset_sticky_state();
        if self.cursor_col > 0 {
            // Atomic marker jump: when a marker ends exactly at the cursor, step
            // over the whole token in one move instead of into its text.
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

    /// Moves the cursor right by one grapheme, wrapping to the start of the next
    /// line at end-of-line. Clears the sticky column.
    fn move_right(&mut self) {
        self.reset_sticky_state();
        let line_len = self.current_line().len();
        if self.cursor_col < line_len {
            // Atomic marker jump: when a marker begins exactly at the cursor,
            // step over the whole token in one move.
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

    fn word_boundary_left(&self) -> usize {
        word_left(
            self.current_line(),
            self.cursor_col,
            self.word_classifier.as_ref(),
        )
    }

    fn word_boundary_right(&self) -> usize {
        let line = self.current_line();
        if self.cursor_col >= line.len() {
            return line.len();
        }
        // Two-phase word-right with a marker splice between the phases. Skip a
        // leading separator run, and if a marker begins right after it, jump the
        // whole token atomically. Otherwise fall back to the normal class skip.
        // Splicing here keeps the marker atomic for Alt-F without a second copy
        // of the traversal. Word-left has no such branch, matching the
        // asymmetry of the reference editor.
        let after_sep = skip_separators(line, self.cursor_col, self.word_classifier.as_ref());
        if let Some((_start, end)) = marker_starting_at(line, after_sep, &self.pastes) {
            return end;
        }
        skip_class(line, after_sep, self.word_classifier.as_ref())
    }

    /// Moves one word left, wrapping to the end of the previous line at column
    /// zero.
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

    /// Moves one word right, wrapping to the start of the next line at
    /// end-of-line.
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

    /// Jumps the cursor to the next (forward) or previous (backward) occurrence
    /// of `needle` in the document, scanning across logical lines.
    ///
    /// Forward scans from just after the cursor to end of line, then each later
    /// line from column zero. Backward scans from just before the cursor to
    /// column zero, then each earlier line in full. The search is
    /// case-sensitive. A match moves the cursor, clears sticky state, and resets
    /// `last_action` so a following type starts a fresh undo unit. No match
    /// leaves everything untouched, so an unsuccessful jump does not disturb an
    /// ongoing undo chain.
    fn jump_to_char(&mut self, needle: char, direction: JumpDirection) {
        let mut buf = [0u8; 4];
        let needle_str: &str = needle.encode_utf8(&mut buf);

        match direction {
            JumpDirection::Forward => {
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
        // No match: leave the cursor and chain state untouched.
    }

    // -- Wrapping --

    /// Atomic display segments of `line`. One grapheme per segment, except that
    /// each valid paste-marker token is a single segment.
    ///
    /// The wrapper and the vertical-move snap consume segments as indivisible
    /// units, so emitting a whole marker as one segment is what keeps a marker
    /// from being split across a wrap boundary or entered mid-token.
    fn segment_line<'a>(&self, line: &'a str) -> Vec<AtomicSegment<'a>> {
        // Fast path: no paste bookkeeping, or no marker-shaped content, means
        // the line is indistinguishable from its grapheme list.
        if self.pastes.is_empty() || !line.contains(PASTE_MARKER_PREFIX) {
            return line
                .grapheme_indices(true)
                .map(|(i, g)| AtomicSegment {
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
                .map(|(i, g)| AtomicSegment {
                    text: g,
                    start_index: i,
                })
                .collect();
        }

        let mut result: Vec<AtomicSegment<'a>> = Vec::new();
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
                // Emit the whole marker once, at its start, then skip its
                // interior graphemes so the token stays a single segment.
                if i == ms {
                    result.push(AtomicSegment {
                        text: &line[ms..me],
                        start_index: ms,
                    });
                }
                continue;
            }
            result.push(AtomicSegment {
                text: g,
                start_index: i,
            });
        }
        result
    }

    /// Greedy word-wrap of `line` at `width`, returning `(start, end)` byte
    /// spans whose concatenation reconstructs `line` exactly.
    ///
    /// The walk counts display columns and remembers the last
    /// whitespace-to-non-whitespace transition as a wrap opportunity. On
    /// overflow it backtracks to that opportunity when the run since it still
    /// fits, otherwise force-breaks at the current segment. A single grapheme
    /// wider than `width` stays whole on its own row. A wider-than-`width` atom
    /// that is not a single grapheme (a paste marker) sub-splits at grapheme
    /// granularity while staying one logical atom for cursor and snap purposes.
    fn wrap_line_spans(&self, line: &str, width: usize) -> Vec<(usize, usize)> {
        let width = width.max(1);
        let segments = self.segment_line(line);
        self.wrap_segments(line, width, &segments)
    }

    /// The core wrap loop over a pre-built atomic-segment list. See
    /// [`TextArea::wrap_line_spans`] for the algorithm.
    fn wrap_segments(
        &self,
        line: &str,
        width: usize,
        segments: &[AtomicSegment],
    ) -> Vec<(usize, usize)> {
        let mut chunks: Vec<(usize, usize)> = Vec::new();
        let mut current_width: usize = 0;
        let mut chunk_start: usize = 0;

        // Byte offset and running width at the last wrap opportunity.
        let mut wrap_opp_index: Option<usize> = None;
        let mut wrap_opp_width: usize = 0;

        for (i, seg) in segments.iter().enumerate() {
            let char_index = seg.start_index;
            let grapheme = seg.text;
            let g_width = self.measure(grapheme);
            let is_ws = is_segment_whitespace(grapheme);

            // Overflow check runs before advancing the running width.
            if current_width + g_width > width {
                // Backtrack to the last opportunity when the trailing run since
                // it, plus this segment, still fits. Otherwise force-break here.
                let backtrack =
                    wrap_opp_index.filter(|_| current_width + g_width - wrap_opp_width <= width);
                if let Some(opp) = backtrack {
                    chunks.push((chunk_start, opp));
                    chunk_start = opp;
                    current_width -= wrap_opp_width;
                } else if chunk_start < char_index {
                    chunks.push((chunk_start, char_index));
                    chunk_start = char_index;
                    current_width = 0;
                }
                wrap_opp_index = None;
            }

            // An atom wider than the whole width. A single grapheme cannot split
            // further and stays whole, but a wider multi-grapheme atom (a paste
            // marker in a narrow terminal) sub-splits at grapheme granularity so
            // it breaks across rows. The split is purely visual: the atom stays
            // one logical unit for the cursor, word motion, and the snap. All
            // but the last sub-span become finished rows, and the last is the
            // leading edge of the next row.
            if g_width > width {
                let sub = self.wrap_wide_atom(grapheme, char_index, width);
                for &(s, e) in sub.iter().take(sub.len().saturating_sub(1)) {
                    chunks.push((s, e));
                }
                let &(last_start, last_end) = sub
                    .last()
                    .expect("wrap_wide_atom returns at least one span");
                chunk_start = last_start;
                current_width = self.measure(&line[last_start..last_end]);
                wrap_opp_index = None;
                continue;
            }

            current_width += g_width;

            // A wrap opportunity is the boundary after a whitespace run, right
            // before the next non-whitespace segment.
            if is_ws
                && let Some(next) = segments.get(i + 1)
                && !is_segment_whitespace(next.text)
            {
                wrap_opp_index = Some(next.start_index);
                wrap_opp_width = current_width;
            }
        }

        chunks.push((chunk_start, line.len()));
        chunks
    }

    /// Sub-splits an over-wide atom into byte spans at grapheme granularity,
    /// offset by `base` into the logical line.
    ///
    /// Used only for an atom wider than `width` (a paste marker in a narrow
    /// terminal). The split re-wraps the atom's own graphemes as plain single
    /// segments, so it never re-detects the marker and never recurses without
    /// bound: a lone grapheme wider than `width` is the base case and stays
    /// whole.
    fn wrap_wide_atom(&self, atom: &str, base: usize, width: usize) -> Vec<(usize, usize)> {
        let graphemes: Vec<AtomicSegment> = atom
            .grapheme_indices(true)
            .map(|(i, g)| AtomicSegment {
                text: g,
                start_index: i,
            })
            .collect();
        let spans = if graphemes.len() <= 1 {
            vec![(0, atom.len())]
        } else {
            self.wrap_segments(atom, width, &graphemes)
        };
        spans
            .into_iter()
            .map(|(s, e)| (base + s, base + e))
            .collect()
    }

    /// Builds the visual-line map for the whole document at `width`.
    ///
    /// An empty logical line yields one zero-length visual line. A line that
    /// fits yields one visual line spanning its content. Wider lines are
    /// word-wrapped.
    ///
    /// Pure over the document: callers that hit this per frame should go
    /// through [`TextArea::visual_line_map`], which caches the result.
    fn compute_visual_line_map(&self, width: usize) -> Vec<VisualLine> {
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
            if self.measure(line) <= width {
                visual_lines.push(VisualLine {
                    logical_line: i,
                    start_col: 0,
                    length: line.len(),
                });
                continue;
            }
            for (start, end) in self.wrap_line_spans(line, width) {
                visual_lines.push(VisualLine {
                    logical_line: i,
                    start_col: start,
                    length: end - start,
                });
            }
        }

        visual_lines
    }

    /// Returns the visual-line map for the whole document at `width`, rebuilding
    /// and caching it when the cache is empty or was built at a different width.
    ///
    /// The width is clamped to at least 1 (matching `compute_visual_line_map`),
    /// so the stored key equals the width the map was actually built at.
    ///
    /// Contract: the returned slice is valid only while the document is
    /// unchanged. A caller must not hold it across a mutation of `self.lines`.
    /// Every edit drops the cache through `invalidate_visual_line_cache`, so a
    /// borrow taken after an edit reflects the current document.
    fn visual_line_map(&mut self, width: usize) -> &[VisualLine] {
        let width = width.max(1);
        if self.visual_line_cache.as_ref().map(|c| c.width) != Some(width) {
            let lines = self.compute_visual_line_map(width);
            self.visual_line_cache = Some(VisualLineCache { width, lines });
        }
        &self
            .visual_line_cache
            .as_ref()
            .expect("cache populated above")
            .lines
    }

    /// The cached map as a `&self` borrow, without touching the cache.
    ///
    /// The caller must have primed the cache with [`TextArea::visual_line_map`]
    /// at the intended width first. We hand out a shared borrow (rather than the
    /// mutable-borrow-derived slice from `visual_line_map`) so `draw` can read
    /// `self.lines` while iterating the map: both are then shared borrows of
    /// `self` and coexist.
    fn cached_visual_line_map(&self) -> &[VisualLine] {
        &self
            .visual_line_cache
            .as_ref()
            .expect("visual-line cache primed by visual_line_map")
            .lines
    }

    /// Drops the cached visual-line map.
    ///
    /// Called at every site that mutates the logical-line buffer, so a stale map
    /// can never be served. Over-invalidating is harmless: it forces one lazy
    /// rebuild on the next `visual_line_map` call.
    fn invalidate_visual_line_cache(&mut self) {
        self.visual_line_cache = None;
    }

    /// Index into `vls` of the visual line containing `(line, col)`. For the
    /// last segment of a logical line, a cursor exactly at `length` (end of
    /// line) counts as contained. Falls back to the last visual line if nothing
    /// matches (a stale-map guard, not a hot path).
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

    fn find_current_visual_line(&self, vls: &[VisualLine]) -> usize {
        self.find_visual_line_at(vls, self.cursor_line, self.cursor_col)
    }

    /// Clears every piece of sticky-column state.
    ///
    /// Called from any action that is not a vertical cursor move (edits,
    /// horizontal moves, line-endpoint jumps) so the next vertical move captures
    /// a fresh anchor. One site so a new horizontal-ish operation only needs to
    /// touch one name to behave correctly.
    fn reset_sticky_state(&mut self) {
        self.preferred_visual_col = None;
        self.snapped_from_cursor_col = None;
    }

    /// Applies the sticky-column decision table and returns the byte column into
    /// the target visual line where the next vertical move should land.
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
    /// Where P = preferred col is set, S = cursor in middle of source line, T =
    /// target shorter than current visual col, U = target shorter than
    /// preferred col.
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
                // Cases 2 and 7: remember where we wanted to be, land at the
                // target's short end.
                self.preferred_visual_col = Some(current_visual_col);
                return target_max_visual_col;
            }
            // Cases 1 and 6: clear preferred only. We leave
            // `snapped_from_cursor_col` alone because the caller's snap scan may
            // set it, and even if not, a following vertical move still needs the
            // pre-snap anchor from the previous step.
            self.preferred_visual_col = None;
            return current_visual_col;
        }

        let preferred = self.preferred_visual_col.expect("has_preferred checked");
        let target_cant_fit_preferred = target_max_visual_col < preferred;
        if target_too_short || target_cant_fit_preferred {
            // Cases 4 and 5: keep preferred, land at the target's end.
            return target_max_visual_col;
        }

        // Case 3: land exactly on preferred, then clear it.
        self.preferred_visual_col = None;
        preferred
    }

    /// Moves the cursor to `target_visual_line`, honoring the sticky column and
    /// snapping onto an atomic segment (a multi-byte grapheme) the byte-column
    /// math would otherwise land inside.
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

        // Source visual column: use the pre-snap position when the last vertical
        // move snapped onto an atomic segment, so the decision table sees the
        // user's original intent rather than the snapped offset.
        let current_visual_col = if let Some(snapped) = self.snapped_from_cursor_col {
            let vl_idx = self.find_visual_line_at(vls, current_vl.logical_line, snapped);
            snapped.saturating_sub(vls[vl_idx].start_col)
        } else {
            self.cursor_col.saturating_sub(current_vl.start_col)
        };

        // Max columns: on a non-last segment of a logical line the cursor
        // cannot sit past `length - 1`, since that position belongs to the next
        // visual line. On the final segment it can sit at `length` (end of
        // line).
        let is_last_source_segment = current_visual_line == vls.len() - 1
            || vls[current_visual_line + 1].logical_line != current_vl.logical_line;
        let source_max_visual_col = if is_last_source_segment {
            current_vl.length
        } else {
            current_vl.length.saturating_sub(1)
        };

        let is_last_target_segment = target_visual_line == vls.len() - 1
            || vls[target_visual_line + 1].logical_line != target_vl.logical_line;
        let target_max_visual_col = if is_last_target_segment {
            target_vl.length
        } else {
            target_vl.length.saturating_sub(1)
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

        // Atomic-segment snap: if the cursor landed inside a multi-byte segment,
        // snap it to the segment start so it never sits mid-grapheme. When
        // moving down into a continuation visual line of a segment that began on
        // an earlier row, skip forward past the continuation rows so the cursor
        // keeps making progress.
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
            // Cursor is strictly inside a multi-byte segment.
            let is_continuation = seg.start_index < target_vl.start_col;
            let is_moving_down = target_visual_line > current_visual_line;

            if is_continuation && is_moving_down {
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

            // Snap to the segment start, recording the pre-snap cursor so the
            // next vertical move resolves the sticky column against it.
            self.snapped_from_cursor_col = Some(self.cursor_col);
            self.cursor_col = seg.start_index;
            return;
        }

        // No snap: we exited whatever segment we were on.
        self.snapped_from_cursor_col = None;
    }

    /// Moves the cursor up one visual line.
    fn move_up(&mut self) {
        // We must not hold the cached map borrow across `move_to_visual_line`
        // (it takes `&mut self`), so copy the map out. This is a cache hit while
        // the document is unchanged, so it avoids the expensive rebuild even
        // though it clones the small `VisualLine` vec.
        let vls = self.visual_line_map(self.layout_width).to_vec();
        if vls.is_empty() {
            return;
        }
        let current = self.find_current_visual_line(&vls);
        if current == 0 {
            return;
        }
        self.move_to_visual_line(&vls, current, current - 1);
    }

    /// Moves the cursor down one visual line.
    fn move_down(&mut self) {
        let vls = self.visual_line_map(self.layout_width).to_vec();
        if vls.is_empty() {
            return;
        }
        let current = self.find_current_visual_line(&vls);
        if current + 1 >= vls.len() {
            return;
        }
        self.move_to_visual_line(&vls, current, current + 1);
    }

    /// Moves the cursor one page up (`direction < 0`) or down within the
    /// visual-line map. The page size is the last-drawn visible window. The
    /// target row is clamped so paging past an edge is a no-op.
    fn page_scroll(&mut self, direction: i32) {
        self.last_action = LastAction::None;
        let vls = self.visual_line_map(self.layout_width).to_vec();
        if vls.is_empty() {
            return;
        }
        let page_size = self.last_visible_rows.max(1);
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

    // -- Insert / delete --

    /// Inserts a newline at the cursor, splitting the current line.
    fn insert_newline_internal(&mut self) {
        self.invalidate_visual_line_cache();
        // A hard line break leaves any symbol context on the previous line
        // behind, so it closes the popup.
        self.cancel_autocomplete();
        let rest = self.lines[self.cursor_line][self.cursor_col..].to_string();
        self.lines[self.cursor_line].truncate(self.cursor_col);
        self.cursor_line += 1;
        self.lines.insert(self.cursor_line, rest);
        self.cursor_col = 0;
    }

    /// Inserts one character at the cursor with fish-style undo coalescing.
    fn insert_char(&mut self, c: char) {
        self.invalidate_visual_line_cache();
        // Coalescing rule: consecutive non-whitespace characters share one undo
        // unit, while every whitespace character pushes its own snapshot. The
        // word after whitespace does not push a fresh snapshot because
        // `last_action` stays `TypeWord` through the space, so undoing "hello
        // world" takes two steps (" world", then "hello").
        if c.is_whitespace() || self.last_action != LastAction::TypeWord {
            self.save_undo();
        }
        self.lines[self.cursor_line].insert(self.cursor_col, c);
        self.cursor_col += c.len_utf8();
        self.last_action = LastAction::TypeWord;
        self.reset_sticky_state();
        self.maybe_trigger_autocomplete_on_insert(c);
    }

    /// Deletes one grapheme backward, merging with the previous line at column
    /// zero.
    fn backspace(&mut self) {
        self.invalidate_visual_line_cache();
        if self.cursor_col > 0 {
            self.save_undo();
            let old_col = self.cursor_col;
            self.move_left();
            self.lines[self.cursor_line].drain(self.cursor_col..old_col);
            self.last_action = LastAction::None;
            self.reset_sticky_state();
            self.maybe_retrigger_autocomplete_after_delete();
        } else if self.cursor_line > 0 {
            self.save_undo();
            let current = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
            self.lines[self.cursor_line].push_str(&current);
            self.last_action = LastAction::None;
            self.reset_sticky_state();
            self.maybe_retrigger_autocomplete_after_delete();
        }
    }

    /// Deletes one grapheme forward, merging with the next line at end-of-line.
    fn delete_forward(&mut self) {
        self.invalidate_visual_line_cache();
        let line_len = self.current_line().len();
        if self.cursor_col < line_len {
            self.save_undo();
            // Atomic marker delete: when a marker begins at the cursor, drain
            // the whole token in one step rather than a single grapheme of it.
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
            self.maybe_retrigger_autocomplete_after_delete();
        } else if self.cursor_line < self.lines.len() - 1 {
            self.save_undo();
            let next = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].push_str(&next);
            self.last_action = LastAction::None;
            self.reset_sticky_state();
            self.maybe_retrigger_autocomplete_after_delete();
        }
    }

    // -- Kill ring --
    //
    // Backward kills prepend to the current ring entry, forward kills append.
    // Consecutive kills accumulate into one entry when the previous action was
    // also a kill.

    /// Kills from the cursor to end of line, or the newline when already there.
    fn kill_to_end(&mut self) {
        self.invalidate_visual_line_cache();
        let line_len = self.current_line().len();
        if self.cursor_col >= line_len {
            if self.cursor_line < self.lines.len() - 1 {
                self.save_undo();
                let next = self.lines.remove(self.cursor_line + 1);
                self.kill_ring
                    .push("\n", false, self.last_action == LastAction::Kill);
                self.lines[self.cursor_line].push_str(&next);
                self.last_action = LastAction::Kill;
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
    }

    /// Kills from the cursor to start of line, or merges with the previous line
    /// when already there.
    fn kill_to_start(&mut self) {
        self.invalidate_visual_line_cache();
        if self.cursor_col == 0 {
            if self.cursor_line > 0 {
                self.save_undo();
                let current = self.lines.remove(self.cursor_line);
                self.cursor_line -= 1;
                self.cursor_col = self.lines[self.cursor_line].len();
                self.kill_ring
                    .push("\n", true, self.last_action == LastAction::Kill);
                self.lines[self.cursor_line].push_str(&current);
                self.last_action = LastAction::Kill;
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
    }

    /// Kills the word before the cursor, or merges with the previous line at
    /// column zero.
    fn kill_word_backward(&mut self) {
        self.invalidate_visual_line_cache();
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
    }

    /// Kills the word after the cursor, or merges the next line at end-of-line.
    fn kill_word_forward(&mut self) {
        self.invalidate_visual_line_cache();
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
    }

    /// Yanks the most recent kill-ring entry at the cursor.
    fn yank(&mut self) {
        if let Some(text) = self.kill_ring.peek().map(str::to_string) {
            self.save_undo();
            self.insert_yanked_text(&text);
            self.last_action = LastAction::Yank;
            self.reset_sticky_state();
        }
    }

    /// Cycles the kill ring: replaces the just-yanked text with the next entry.
    ///
    /// Only valid immediately after a yank or another yank-pop, and a no-op with
    /// fewer than two entries.
    fn yank_pop(&mut self) {
        if self.last_action != LastAction::Yank || self.kill_ring.len() <= 1 {
            return;
        }
        // The yanked text is still at the ring head (rotate has not run yet), so
        // it tells us what to delete.
        let previous = match self.kill_ring.peek() {
            Some(s) => s.to_string(),
            None => return,
        };
        self.save_undo();
        self.delete_yanked_text(&previous);
        self.kill_ring.rotate();
        if let Some(next) = self.kill_ring.peek().map(str::to_string) {
            self.insert_yanked_text(&next);
        }
        self.last_action = LastAction::Yank;
        self.reset_sticky_state();
    }

    /// Inserts `text` at the cursor, handling embedded newlines, leaving the
    /// cursor just after the inserted region. Shared by yank and yank-pop.
    fn insert_yanked_text(&mut self, text: &str) {
        self.invalidate_visual_line_cache();
        for ch in text.chars() {
            if ch == '\n' {
                self.insert_newline_internal();
            } else {
                self.lines[self.cursor_line].insert(self.cursor_col, ch);
                self.cursor_col += ch.len_utf8();
            }
        }
    }

    /// Removes `text` ending at the cursor, reversing the last yank so yank-pop
    /// can replace it without disturbing surrounding content.
    fn delete_yanked_text(&mut self, text: &str) {
        self.invalidate_visual_line_cache();
        let yank_lines: Vec<&str> = text.split('\n').collect();
        if yank_lines.len() == 1 {
            let byte_len = text.len();
            let start_col = self.cursor_col.saturating_sub(byte_len);
            self.lines[self.cursor_line].drain(start_col..self.cursor_col);
            self.cursor_col = start_col;
            return;
        }

        // Multi-line: the cursor is at the end of the last yanked line. The yank
        // started `n - 1` lines up, at the column where `yank_lines[0]` was
        // appended to that line's original content.
        let n = yank_lines.len();
        let start_line = self.cursor_line.saturating_sub(n - 1);
        let first_yank = yank_lines.first().copied().unwrap_or("");
        let start_col = self.lines[start_line]
            .len()
            .saturating_sub(first_yank.len());

        let before: String = self.lines[start_line][..start_col].to_string();
        let after: String = self.lines[self.cursor_line][self.cursor_col..].to_string();

        self.lines.drain(start_line..=self.cursor_line);
        self.lines.insert(start_line, before.clone() + &after);

        self.cursor_line = start_line;
        self.cursor_col = before.len();
    }

    // -- History navigation --

    /// Navigates history upward.
    ///
    /// The first call (not yet browsing) saves an undo snapshot and parks the
    /// current text as a draft at the tail of the ring. That draft is what
    /// [`TextArea::history_down`] returns to when walking past the newest real
    /// entry. Later calls do not push further snapshots, so one undo returns to
    /// the pre-browse state no matter how far the user walked.
    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_index {
            None => {
                self.save_undo();
                self.history.push(self.text());
                self.history.len() - 2
            }
            Some(i) if i > 0 => i - 1,
            _ => return,
        };
        self.history_index = Some(idx);
        self.load_history_entry(idx);
    }

    /// Navigates history downward, restoring and dropping the draft when it
    /// walks back past the newest real entry.
    fn history_down(&mut self) {
        let idx = match self.history_index {
            Some(i) => i + 1,
            None => return,
        };
        if idx >= self.history.len() {
            return;
        }
        self.history_index = if idx == self.history.len() - 1 {
            None
        } else {
            Some(idx)
        };
        let text = self.history[idx].clone();
        if self.history_index.is_none() {
            self.history.pop();
        }
        self.set_document(&text);
    }

    fn load_history_entry(&mut self, idx: usize) {
        let text = self.history[idx].clone();
        self.set_document(&text);
    }

    /// Replaces the document with `text` (already normalized), cursor to end.
    /// Used by history navigation, which owns the ring shape itself.
    fn set_document(&mut self, text: &str) {
        self.invalidate_visual_line_cache();
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_string).collect()
        };
        self.cursor_line = self.lines.len() - 1;
        self.cursor_col = self.lines[self.cursor_line].len();
    }

    // -- Submit --

    /// Whether the character immediately before the cursor is a literal `\`.
    fn cursor_preceded_by_backslash(&self) -> bool {
        if self.cursor_col == 0 {
            return false;
        }
        self.current_line()[..self.cursor_col].chars().next_back() == Some('\\')
    }

    /// Whether the cursor sits on the first line at an empty-or-`/`-only
    /// prefix. Restricts the palette trigger to the start of a message.
    fn is_at_start_of_message(&self) -> bool {
        if self.cursor_line != 0 {
            return false;
        }
        let trimmed = self.lines[0][..self.cursor_col].trim();
        trimmed.is_empty() || trimmed == "/"
    }

    /// Fires `on_submit` with the trimmed contents, records the submitted text
    /// for polling, then resets to an empty document with a clean undo stack.
    ///
    /// The submitted value is the paste-marker-expanded text, so a consumer sees
    /// the literal pasted bytes rather than the marker placeholder.
    fn submit_value(&mut self, ctx: &mut EventContext) {
        self.invalidate_visual_line_cache();
        let text = self.expanded_text().trim().to_string();
        self.submitted_text = Some(text.clone());
        if let Some(cb) = self.on_submit.as_mut() {
            cb(ctx, &text);
        }
        self.lines = vec![String::new()];
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.reset_sticky_state();
        self.undo_stack.clear();
        // Submit is a hard break: the paste content has been consumed into the
        // submitted value, so drop the whole paste state and let the counter
        // start fresh for the next message.
        self.pastes.clear();
        self.paste_counter = 0;
        self.last_action = LastAction::None;
        self.history_index = None;
    }

    /// Consumes the event with a redraw, then fires `on_change` if the text
    /// actually changed since the last check.
    ///
    /// Every handled key routes through here so cursor-only moves still redraw
    /// but do not fire `on_change`. Programmatic setters bypass it, so
    /// `on_change` reflects interactive edits only.
    fn check_changed(&mut self, ctx: &mut EventContext) {
        ctx.consume_and_redraw();
        if self.on_change.is_none() {
            return;
        }
        let new = self.text();
        if new != self.previous_val {
            if let Some(cb) = self.on_change.as_mut() {
                cb(ctx, &new);
            }
        }
        self.previous_val = new;
    }

    // -- Vertical dispatch helpers --

    /// Handles an Up press: history at an empty or already-browsing top line,
    /// jump-to-start on an otherwise top line, else move up one visual line.
    fn on_cursor_up(&mut self) {
        // Prime the cache, then read the one value we need off a shared borrow so
        // the borrow is dropped before we delegate to a `&mut self` mover below.
        self.visual_line_map(self.layout_width);
        let current_vl = {
            let vls = self.cached_visual_line_map();
            self.find_current_visual_line(vls)
        };
        if self.is_empty_doc() {
            self.history_up();
        } else if self.history_index.is_some() && current_vl == 0 {
            self.history_up();
        } else if current_vl == 0 {
            self.cursor_col = 0;
            self.reset_sticky_state();
        } else {
            self.move_up();
        }
        self.last_action = LastAction::None;
    }

    /// Handles a Down press: symmetric to [`TextArea::on_cursor_up`].
    fn on_cursor_down(&mut self) {
        self.visual_line_map(self.layout_width);
        let on_last_vl = {
            let vls = self.cached_visual_line_map();
            let current = self.find_current_visual_line(vls);
            current + 1 >= vls.len()
        };
        if self.history_index.is_some() && on_last_vl {
            self.history_down();
        } else if on_last_vl {
            self.cursor_col = self.current_line().len();
            self.reset_sticky_state();
        } else {
            self.move_down();
        }
        self.last_action = LastAction::None;
    }

    // -- Autocomplete: engine --

    /// Cancels the popup and any pending work: clears popup state, drops the
    /// streaming session (whose `Drop` stops its worker), and cancels the
    /// one-shot request. Idempotent.
    fn cancel_autocomplete(&mut self) {
        self.autocomplete_state = None;
        self.autocomplete_items.clear();
        self.autocomplete_selected = 0;
        self.autocomplete_prefix.clear();
        self.autocomplete_session = None;
        self.cancel_pending_autocomplete_request();
    }

    /// Cancels the in-flight one-shot request without touching popup state.
    ///
    /// Tripping the token is what stops a spawned worker: dropping the
    /// `JoinHandle` does not abort the task, so we rely on the worker checking
    /// the token. Tests may still await the handle to sync with termination.
    fn cancel_pending_autocomplete_request(&mut self) {
        if let Some(token) = self.autocomplete_cancel.take() {
            token.cancel();
        }
        self.autocomplete_task = None;
        // Advance the request id so a delivery already past its cancel check no
        // longer matches. A worker races between observing the token and its
        // send (no await sits between those two, so the token can fire in
        // between), and a bare cancel (Esc, a provider swap) edits neither the
        // buffer nor the cursor, so the snapshot guard would let the late
        // delivery through. Bumping the id here is what actually drops it, so
        // the popup never resurrects after Esc and an old provider's result
        // never applies under a newly installed one.
        self.autocomplete_request_id = self.autocomplete_request_id.wrapping_add(1);
    }

    /// Asks the provider for suggestions and updates the popup. `force` drives
    /// the Tab path (stays open on narrow); otherwise the trigger is implicit.
    ///
    /// Streaming-first: an existing session absorbs the new cursor, or a new one
    /// opens through [`AutocompleteProvider::try_start_session`]. Only when no
    /// session serves the context does this fall back to the one-shot async
    /// dispatch.
    fn update_autocomplete(&mut self, force: bool) {
        let Some(provider) = self.autocomplete_provider.clone() else {
            return;
        };

        let mode = if force {
            AutocompleteMode::Force
        } else {
            // Keep an existing Force popup in Force mode across a narrowing
            // keystroke so its stays-open close semantics do not downgrade.
            match self.autocomplete_state {
                Some(AutocompleteMode::Force) => AutocompleteMode::Force,
                _ => AutocompleteMode::Regular,
            }
        };

        // Path 1: an existing session absorbs the new cursor. On failure we
        // drop it and fall through to open a fresh one.
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

        // Path 2: open a new streaming session for this context.
        let notify = self.make_autocomplete_notify();
        if let Some(session) =
            provider.try_start_session(&self.lines, self.cursor_line, self.cursor_col, notify)
        {
            // A session in charge makes any pending one-shot delivery moot.
            self.cancel_pending_autocomplete_request();
            self.autocomplete_prefix = session.prefix().to_string();
            self.autocomplete_session = Some(session);
            self.autocomplete_state = Some(mode);
            // Cleared so the popup does not show a previous session's snapshot;
            // the next pump repopulates it.
            self.autocomplete_items.clear();
            self.autocomplete_selected = 0;
            return;
        }

        // Path 3: one-shot async dispatch.
        self.dispatch_autocomplete_request(mode, false);
    }

    /// Spawns a worker that runs the provider off the widget thread and delivers
    /// the result down `autocomplete_tx`. Bumps the request id and cancels the
    /// prior request first, so only the newest delivery ever applies.
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
            text: self.text(),
            cursor_line: self.cursor_line,
            cursor_col: self.cursor_col,
        };
        let lines = self.lines.clone();
        let cursor_line = self.cursor_line;
        let cursor_col = self.cursor_col;
        let tx = self.autocomplete_tx.clone();
        let force = matches!(mode, AutocompleteMode::Force);

        // Only the implicit `@` / `#` symbol path debounces; Tab and everything
        // else fire immediately.
        let debounce = self.autocomplete_debounce_for(mode);

        // !Send boundary: the widget is `Rc<RefCell<..>>` and !Send, so it must
        // never be captured here. The task closes over Send data only (the
        // provider `Arc`, the snapshot, the sender, the ids, the cancel token,
        // the line clone). Capturing `self` would fail the `spawn` Send bound,
        // which is the compile-time proof that this stays clean.
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

            // Fire-and-forget: a closed receiver means the widget is gone and
            // there is nothing useful left to do.
            let _ = tx.send(AutocompleteDelivery::query(
                request_id,
                snapshot,
                suggestions,
                mode,
                auto_apply_single,
            ));
        });

        self.autocomplete_task = Some(task);
    }

    /// The debounce for a request: a coalescing window for the implicit symbol
    /// path, zero for Tab (force) and non-symbol contexts.
    fn autocomplete_debounce_for(&self, mode: AutocompleteMode) -> Duration {
        if matches!(mode, AutocompleteMode::Force) {
            return Duration::ZERO;
        }
        if self.is_in_symbol_context() {
            ATTACHMENT_AUTOCOMPLETE_DEBOUNCE
        } else {
            Duration::ZERO
        }
    }

    /// Builds the wake a streaming session invokes from its worker threads.
    ///
    /// notify -> delivery wake: the worker cannot touch the !Send widget, so it
    /// only pushes a marker down the same channel the host already drains. The
    /// host wakes, calls `pump_autocomplete`, and the session is ticked on the
    /// widget's own thread. The closure is `Arc<dyn Fn + Send + Sync>` because
    /// sessions may call it from any worker thread.
    fn make_autocomplete_notify(&self) -> Arc<dyn Fn() + Send + Sync> {
        let tx = self.autocomplete_tx.clone();
        Arc::new(move || {
            let _ = tx.send(AutocompleteDelivery::session_progressed());
        })
    }

    /// Ticks the active streaming session and rebuilds the popup when its
    /// snapshot changed. A no-op with no session. Safe to call on every pump.
    fn pump_autocomplete_session(&mut self) {
        // Take the snapshot inside a scope so the `&mut` borrow of the session
        // ends before we read `&self` in `item_matches_typed_prefix` below. An
        // early return here returns from the function, which is why the block
        // can type-check as a plain tuple.
        let (running, items) = {
            let Some(session) = self.autocomplete_session.as_mut() else {
                return;
            };
            let status = session.tick(AUTOCOMPLETE_TICK_BUDGET_MS);
            if !status.changed && !self.autocomplete_items.is_empty() {
                return;
            }
            (status.running, session.snapshot())
        };

        if items.is_empty() {
            // No matches yet. Close a Regular popup once the worker is done (so
            // a stray `@xyz` matching nothing does not linger), but keep a Force
            // popup open so the user can keep narrowing.
            if !running && matches!(self.autocomplete_state, Some(AutocompleteMode::Regular)) {
                self.cancel_autocomplete();
            } else {
                self.autocomplete_items.clear();
                self.autocomplete_selected = 0;
            }
            return;
        }

        let selected = items
            .iter()
            .position(|it| self.item_matches_typed_prefix(it))
            .unwrap_or(0);
        self.autocomplete_items = items;
        self.autocomplete_selected = selected;
    }

    /// Applies a one-shot query result to the popup: auto-applies a lone item on
    /// the force path, otherwise opens (or closes) the popup per its mode.
    fn apply_suggestions(
        &mut self,
        suggestions: Option<AutocompleteSuggestions>,
        mode: AutocompleteMode,
        auto_apply_single: bool,
    ) {
        let Some(suggestions) = suggestions else {
            // No matches. Force keeps the popup open (and empty) so the user can
            // narrow; Regular closes it.
            self.autocomplete_items.clear();
            self.autocomplete_selected = 0;
            self.autocomplete_prefix.clear();
            self.autocomplete_state = match mode {
                AutocompleteMode::Force => Some(AutocompleteMode::Force),
                AutocompleteMode::Regular => None,
            };
            return;
        };

        if auto_apply_single && suggestions.items.len() == 1 {
            let item = suggestions.items[0].clone();
            self.autocomplete_prefix = suggestions.prefix;
            self.apply_autocomplete_item(item);
            return;
        }

        if suggestions.items.is_empty() {
            if !matches!(mode, AutocompleteMode::Force) {
                self.autocomplete_state = None;
                self.autocomplete_items.clear();
                self.autocomplete_selected = 0;
                self.autocomplete_prefix.clear();
            }
            return;
        }

        self.autocomplete_prefix = suggestions.prefix;
        let items = suggestions.items;
        // Pre-highlight the first item whose value extends the typed text so a
        // unique prefix match lights up without further navigation.
        let selected = items
            .iter()
            .position(|it| self.item_matches_typed_prefix(it))
            .unwrap_or(0);
        self.autocomplete_items = items;
        self.autocomplete_selected = selected;
        self.autocomplete_state = Some(mode);
    }

    /// Whether `item`'s value begins with the text typed at the cursor (the
    /// `autocomplete_prefix`-long span ending at the cursor).
    fn item_matches_typed_prefix(&self, item: &AutocompleteItem) -> bool {
        let line = &self.lines[self.cursor_line];
        let prefix_len = self.autocomplete_prefix.len();
        let typed_start = self.cursor_col.saturating_sub(prefix_len);
        let typed = &line[typed_start..self.cursor_col];
        item.value.starts_with(typed)
    }

    /// Force-requests suggestions from Tab when the popup is closed. Honors the
    /// provider's [`AutocompleteProvider::should_trigger_file_completion`] hook.
    /// If exactly one result lands it is applied directly; multiple open the
    /// popup in Force mode.
    fn trigger_force_autocomplete(&mut self) {
        if let Some(provider) = self.autocomplete_provider.as_ref()
            && !provider.should_trigger_file_completion(
                &self.lines,
                self.cursor_line,
                self.cursor_col,
            )
        {
            return;
        }
        self.dispatch_autocomplete_request(AutocompleteMode::Force, true);
    }

    /// Splices `item`'s value over the `autocomplete_prefix`-long span ending at
    /// the cursor, saving one undo step and closing the popup.
    ///
    /// Does not fire `on_change`: it has no `EventContext`. The Tab and Enter
    /// apply paths run inside `handle_key` and follow this with `check_changed`,
    /// so those fire. A delivery-driven auto-apply (a lone force result) runs
    /// without a context, so `on_change` does not fire there. The host requests
    /// a redraw on the delivery instead. No current consumer wires `on_change`
    /// on this widget, so the asymmetry is invisible today.
    fn apply_autocomplete_item(&mut self, item: AutocompleteItem) {
        let Some(provider) = self.autocomplete_provider.clone() else {
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
        self.invalidate_visual_line_cache();
        self.cursor_line = result.cursor_line;
        self.cursor_col = result.cursor_col;
        self.cancel_autocomplete();
        self.reset_sticky_state();
        self.last_action = LastAction::None;
    }

    /// Moves the popup highlight one row, clamped to the item bounds.
    fn move_autocomplete_selection(&mut self, forward: bool) {
        let len = self.autocomplete_items.len();
        if len == 0 {
            return;
        }
        if forward {
            self.autocomplete_selected = (self.autocomplete_selected + 1).min(len - 1);
        } else {
            self.autocomplete_selected = self.autocomplete_selected.saturating_sub(1);
        }
    }

    /// Applies the highlighted item (Tab on an open popup). Closes the popup if
    /// there is nothing to apply.
    fn apply_selected_autocomplete(&mut self) {
        if self.autocomplete_items.is_empty() {
            self.cancel_autocomplete();
            return;
        }
        let item = self.autocomplete_items[self.autocomplete_selected].clone();
        self.apply_autocomplete_item(item);
    }

    /// Resolves Enter on an open popup: if the typed text is exactly one item's
    /// value, keep it verbatim and close; otherwise apply the highlighted item.
    /// Always consumes the keystroke, so Enter on a popup never submits.
    fn accept_autocomplete_on_enter(&mut self) {
        if self.autocomplete_items.is_empty() {
            self.cancel_autocomplete();
            return;
        }
        let line = &self.lines[self.cursor_line];
        let prefix_len = self.autocomplete_prefix.len();
        let typed_start = self.cursor_col.saturating_sub(prefix_len);
        let typed = line[typed_start..self.cursor_col].to_string();
        let exact = self.autocomplete_items.iter().any(|i| i.value == typed);
        if exact {
            self.cancel_autocomplete();
        } else {
            let item = self.autocomplete_items[self.autocomplete_selected].clone();
            self.apply_autocomplete_item(item);
        }
    }

    /// Called after inserting one `char`. Opens the popup only when the typed
    /// character plausibly starts or continues a completable context:
    ///
    /// - `@` / `#` right after whitespace or start-of-line opens a symbol popup.
    /// - an identifier char inside an existing `@` / `#` context refines it.
    ///
    /// Any other character (plain prose, whitespace, a `/` that is not the
    /// palette trigger) opens nothing. A `/` is not a symbol context char, so
    /// prose never fires a "list every candidate" query. An already-open popup
    /// refreshes on every insert so narrowing keeps working.
    fn maybe_trigger_autocomplete_on_insert(&mut self, c: char) {
        if self.autocomplete_provider.is_none() {
            return;
        }
        if self.autocomplete_state.is_some() {
            self.refresh_open_autocomplete();
            return;
        }
        let should_trigger = match c {
            '@' | '#' => self.symbol_follows_whitespace_or_start(c),
            c if is_identifier_char(c) => self.is_in_symbol_context(),
            _ => false,
        };
        if should_trigger {
            self.update_autocomplete(false);
        }
    }

    /// Called after a deletion. Refreshes an open popup, or re-opens one only
    /// when the deletion left the cursor inside an `@` / `#` symbol context.
    fn maybe_retrigger_autocomplete_after_delete(&mut self) {
        if self.autocomplete_provider.is_none() {
            return;
        }
        if self.autocomplete_state.is_some() {
            self.refresh_open_autocomplete();
            return;
        }
        if self.is_in_symbol_context() {
            self.update_autocomplete(false);
        }
    }

    /// Re-runs the dispatch for an already-open popup after an edit.
    ///
    /// A `Force` popup (opened via Tab) refines a direct path unconditionally,
    /// so it re-dispatches on every edit. A `Regular` popup is anchored to an
    /// `@` / `#` token: once the cursor leaves that token, for example after
    /// typing a space, the completion no longer applies. We close it rather
    /// than re-dispatch, because a bare re-dispatch would fall through to the
    /// one-shot direct-path branch and list the whole working directory.
    fn refresh_open_autocomplete(&mut self) {
        match self.autocomplete_state {
            Some(AutocompleteMode::Force) => self.update_autocomplete(true),
            _ if self.is_in_symbol_context() => self.update_autocomplete(false),
            _ => self.cancel_autocomplete(),
        }
    }

    /// Whether the just-inserted `@` / `#` sits at a token boundary: the char
    /// before it is whitespace, or it is at the start of the line. Stops `@foo`
    /// or `#bar` inside a word from opening a symbol popup.
    fn symbol_follows_whitespace_or_start(&self, sym: char) -> bool {
        let before = &self.lines[self.cursor_line][..self.cursor_col];
        let mut buf = [0u8; 4];
        let s: &str = sym.encode_utf8(&mut buf);
        let Some(before_sym) = before.strip_suffix(s) else {
            return false;
        };
        match before_sym.chars().last() {
            None => true,
            Some(c) => c == ' ' || c == '\t',
        }
    }

    /// Whether the cursor sits inside an `@`- or `#`-prefixed symbol token. See
    /// [`ends_in_symbol_context`].
    fn is_in_symbol_context(&self) -> bool {
        let before = &self.lines[self.cursor_line][..self.cursor_col];
        ends_in_symbol_context(before)
    }

    // -- Drawing --

    /// Writes one border rule into `surf` at `row`: `inlay` text followed by `─`
    /// padding to `width`, all in `style`, truncated to `width`.
    fn draw_rule(&self, surf: &mut Surface, row: u16, width: u16, inlay: &str, style: Style) {
        let width_usize = usize::from(width);
        let mut s = String::from(inlay);
        let inlay_w = self.measure(inlay);
        for _ in inlay_w..width_usize {
            s.push('─');
        }
        let mut col: u16 = 0;
        for g in s.graphemes(true) {
            let gw = gwidth::gwidth(g, self.width_method);
            if usize::from(col) + usize::from(gw) > width_usize {
                break;
            }
            surf.write_cell(
                col,
                row,
                Cell {
                    char: Character::new(g, u8::try_from(gw).expect("grapheme width fits a u8")),
                    style,
                    ..Cell::default()
                },
            );
            col = col.saturating_add(gw);
        }
    }

    /// The popup's visible window as `(first_item_index, row_count)`, with the
    /// window height clamped to at most `max_rows`.
    ///
    /// `(0, 0)` when the popup is closed, has no items, or `max_rows` is zero.
    /// Otherwise the window is `min(max_visible, len, max_rows)` rows, scrolled
    /// to keep the selected row roughly centered: the start clamps
    /// `selected - count/2` into `[0, len - count]`, so the highlight stays
    /// visible while the list never scrolls past either end.
    ///
    /// The host clamps `max_rows` to the space above the editor, so it can
    /// shrink the window below `max_visible`. Recentering uses the clamped
    /// `count`, which keeps the selected row on screen even for a short window.
    fn autocomplete_popup_window(&self, max_rows: usize) -> (usize, usize) {
        if self.autocomplete_state.is_none() || self.autocomplete_items.is_empty() {
            return (0, 0);
        }
        let len = self.autocomplete_items.len();
        let count = self.autocomplete_max_visible.min(len).min(max_rows);
        if count == 0 {
            return (0, 0);
        }
        let start = if len <= count {
            0
        } else {
            let half = count / 2;
            self.autocomplete_selected
                .saturating_sub(half)
                .min(len - count)
        };
        (start, count)
    }

    /// Draws `popup_count` suggestion rows starting at `first_row`, one item per
    /// row from `popup_start`. Each row is filled edge-to-edge with its style
    /// (so the selected row reads as a band) and shows a single column: the
    /// item's full path, left-aligned at `padding_x`.
    fn draw_autocomplete_popup(
        &self,
        surf: &mut Surface,
        first_row: u16,
        width: u16,
        popup_start: usize,
        popup_count: usize,
    ) {
        let visible = &self.autocomplete_items[popup_start..popup_start + popup_count];

        for (offset, item) in visible.iter().enumerate() {
            let row =
                first_row.saturating_add(u16::try_from(offset).expect("popup row fits a u16"));
            let selected = popup_start + offset == self.autocomplete_selected;
            let style = if selected {
                self.theme.popup.selected
            } else {
                self.theme.popup.item
            };

            // Fill the whole row so the row style paints the full band.
            for c in 0..width {
                surf.write_cell(
                    c,
                    row,
                    Cell {
                        char: Character::new(" ", 1),
                        style,
                        ..Cell::default()
                    },
                );
            }

            // Single column: the item's full path. Fuzzy items carry the full
            // relative path in `description`; direct-path items have no
            // `description` and hold the path fragment in `label`. So we show
            // the description when present, otherwise the label.
            //
            // Preserve the directory affordance: a fuzzy directory item's
            // `description` is the path without a trailing slash while its
            // `label` ends in one, so append the slash when the label marks a
            // directory and the shown text does not already carry it.
            let shown = item.description.as_deref().unwrap_or(item.label.as_str());
            if item.label.ends_with('/') && !shown.ends_with('/') {
                let mut text = shown.to_string();
                text.push('/');
                self.draw_popup_text(surf, row, width, self.padding_x, &text, style);
            } else {
                self.draw_popup_text(surf, row, width, self.padding_x, shown, style);
            }
        }
    }

    /// Writes `text` into `surf` at `row`, starting at column `start_col`, in
    /// `style`, clipped to `width`. Advances by measured grapheme width.
    fn draw_popup_text(
        &self,
        surf: &mut Surface,
        row: u16,
        width: u16,
        start_col: usize,
        text: &str,
        style: Style,
    ) {
        let width_usize = usize::from(width);
        let mut col = u16::try_from(start_col.min(width_usize)).expect("popup col fits a u16");
        for g in text.graphemes(true) {
            let gw = gwidth::gwidth(g, self.width_method);
            if usize::from(col) + usize::from(gw) > width_usize {
                break;
            }
            surf.write_cell(
                col,
                row,
                Cell {
                    char: Character::new(g, u8::try_from(gw).expect("grapheme width fits a u8")),
                    style,
                    ..Cell::default()
                },
            );
            col = col.saturating_add(gw);
        }
    }

    /// The number of autocomplete popup rows to show, capped at `max_rows`.
    ///
    /// `min(autocomplete_max_visible, items.len(), max_rows)`, or `0` when the
    /// popup is closed or empty. The host passes the space available above the
    /// editor so the overlay never overflows the screen.
    pub fn autocomplete_popup_rows(&self, max_rows: usize) -> usize {
        self.autocomplete_popup_window(max_rows).1
    }

    /// A standalone popup surface of `width x rows` for the host to float as an
    /// overlay above the editor, where `rows = autocomplete_popup_rows(max_rows)`.
    ///
    /// Returns `None` when the popup is closed, empty, or `rows` is zero. The
    /// surface holds the single-column full-path rows with the full-row band
    /// fill and the item/selected styles, drawn from its own row 0. The scroll
    /// window recenters on the selected row using the clamped `rows`, so the
    /// selection stays visible even when `max_rows` is smaller than the item
    /// count.
    ///
    /// Reads presentation state the editor's own `draw` sets (`width_method`,
    /// `theme`, the items, `last_visible_rows`), so the host must call it only
    /// after the editor has drawn this frame. The shell layout guarantees that
    /// ordering: the editor draws inside the base column before the host places
    /// the overlay.
    pub fn draw_autocomplete_popup_surface(&self, width: u16, max_rows: usize) -> Option<Surface> {
        let (start, count) = self.autocomplete_popup_window(max_rows);
        if count == 0 {
            return None;
        }
        let height = u16::try_from(count).expect("popup row count fits a u16");
        let mut surf = Surface::with_size(Size { width, height });
        self.draw_autocomplete_popup(&mut surf, 0, width, start, count);
        Some(surf)
    }

    /// The height, in rows, of the editor block drawn by [`draw`](Widget::draw):
    /// the visible input rows plus the top and bottom border rules
    /// (`last_visible_rows + 2`).
    ///
    /// Reflects the last completed draw. The host reads it right after the
    /// editor draws to anchor the autocomplete overlay directly above the block.
    pub fn drawn_height(&self) -> u16 {
        u16::try_from(self.last_visible_rows + 2).expect("editor height fits a u16")
    }
}

impl Widget for TextArea {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let width = ctx
            .max
            .width
            .expect("TextArea requires a bounded max width");
        self.width_method = ctx.width_method;

        let width_usize = usize::from(width);
        let content_width = width_usize.saturating_sub(self.padding_x * 2);
        let border_style = Style {
            fg: self.theme.border_color,
            ..Style::default()
        };

        if content_width == 0 {
            // No room for content: draw just the two border rules.
            let mut surf = Surface::with_size(Size { width, height: 2 });
            self.draw_rule(&mut surf, 0, width, "", border_style);
            self.draw_rule(&mut surf, 1, width, "", border_style);
            return surf;
        }

        // Reserve one column for the caret at end-of-line when there is no
        // padding, so a line that would exactly fill the content still leaves
        // the caret on screen.
        let layout_width = if self.padding_x == 0 {
            content_width.saturating_sub(1).max(1)
        } else {
            content_width
        };
        self.layout_width = layout_width;

        // Visible-row cap from the constraints. The layout owns the height
        // policy; we only clamp to what we are given.
        let available = match ctx.max.height {
            Some(h) => usize::from(h).saturating_sub(2),
            None => usize::MAX,
        };
        let cap = self
            .max_visible_rows
            .map(|m| m.max(1))
            .unwrap_or(available)
            .min(available);

        // Prime the cache at `layout_width`, then take a *shared* borrow of the
        // map so the content loop and cursor block below can read `self.lines`
        // alongside it. `visual_line_map` borrows `self` mutably to (re)build, so
        // we prime it as a separate statement and re-borrow shared here.
        self.visual_line_map(layout_width);
        let vls = self.cached_visual_line_map();
        let total_visual = vls.len();
        let cursor_vl_idx = self.find_current_visual_line(vls);
        let visible_count = total_visual.min(cap);

        // Scroll window: keep the cursor's visual line inside it, then clamp so
        // we never scroll past the last row.
        let mut scroll_start = if cursor_vl_idx < self.scroll_offset {
            cursor_vl_idx
        } else if visible_count > 0 && cursor_vl_idx >= self.scroll_offset + visible_count {
            cursor_vl_idx + 1 - visible_count
        } else {
            self.scroll_offset
        };
        scroll_start = scroll_start.min(total_visual.saturating_sub(visible_count));
        // `scroll_offset` and `last_visible_rows` are written at the end of draw,
        // once `vls` is last used, since writing them mutates `self` while `vls`
        // borrows it.

        // The editor block is independent of the autocomplete popup: the popup
        // is an overlay the host floats above this block (see
        // `draw_autocomplete_popup_surface`), so it never grows this surface.
        //
        // Row layout, top to bottom:
        //   0                              top border rule
        //   [1, visible_count]             input content rows
        //   visible_count + 1              bottom border rule
        let height = u16::try_from(visible_count + 2).expect("editor height fits a u16");
        let mut surf = Surface::with_size(Size { width, height });

        // Top rule with optional scroll indicator and top-bar label.
        let mut top_inlay = String::new();
        if scroll_start > 0 {
            top_inlay.push_str(&format!("─── ↑ {scroll_start} more "));
        }
        if let Some(label) = &self.top_bar_label {
            top_inlay.push_str(&format!("─── {label} "));
        }
        self.draw_rule(&mut surf, 0, width, &top_inlay, border_style);

        // Content rows, below the top rule.
        let padding_col = u16::try_from(self.padding_x).expect("padding fits a u16");
        for (offset, vl_idx) in (scroll_start..scroll_start + visible_count).enumerate() {
            let row = u16::try_from(offset + 1).expect("row fits a u16");
            let vl = vls[vl_idx];
            let line = &self.lines[vl.logical_line];
            let text = &line[vl.start_col..vl.start_col + vl.length];
            let mut col = padding_col;
            for g in text.graphemes(true) {
                let gw = gwidth::gwidth(g, self.width_method);
                if usize::from(col) + usize::from(gw) > width_usize {
                    break;
                }
                surf.write_cell(
                    col,
                    row,
                    Cell {
                        char: Character::new(
                            g,
                            u8::try_from(gw).expect("grapheme width fits a u8"),
                        ),
                        style: Style::default(),
                        ..Cell::default()
                    },
                );
                col = col.saturating_add(gw);
            }
        }

        // Bottom rule with optional `↓ N more` indicator.
        let bottom_row = u16::try_from(visible_count + 1).expect("row fits a u16");
        let lines_below = total_visual.saturating_sub(scroll_start + visible_count);
        let mut bottom_inlay = String::new();
        if lines_below > 0 {
            bottom_inlay.push_str(&format!("─── ↓ {lines_below} more "));
        }
        self.draw_rule(&mut surf, bottom_row, width, &bottom_inlay, border_style);

        // Report the caret when its visual line is inside the window. The
        // framework renders it only while this widget is focused.
        if visible_count > 0
            && cursor_vl_idx >= scroll_start
            && cursor_vl_idx < scroll_start + visible_count
        {
            let vl = vls[cursor_vl_idx];
            debug_assert_eq!(vl.logical_line, self.cursor_line);
            let before = &self.lines[self.cursor_line][vl.start_col..self.cursor_col];
            let cursor_vis_col = self.measure(before);
            let row = u16::try_from(cursor_vl_idx - scroll_start + 1).expect("cursor row fits u16");
            let col = u16::try_from(self.padding_x + cursor_vis_col).expect("cursor col fits u16");
            surf.cursor = Some(CursorState {
                row,
                col,
                shape: CursorShape::Default,
            });
        }

        // Deferred until `vls`'s last use above: these mutate `self`.
        self.last_visible_rows = visible_count;
        self.scroll_offset = scroll_start;

        surf
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            Event::FocusIn | Event::FocusOut => ctx.redraw = true,
            Event::Paste(text) => {
                self.handle_paste(text);
                self.check_changed(ctx);
            }
            Event::KeyPress(key) => self.handle_key(ctx, key),
            _ => {}
        }
    }

    fn wants_events(&self) -> bool {
        true
    }

    fn debug_label(&self) -> &'static str {
        "TextArea"
    }
}

impl TextArea {
    /// Dispatches a key press. The order matters: newline is checked before
    /// submit, word motions before plain motions, and word deletes before char
    /// deletes, so a modified key never falls through to the plainer chord.
    fn handle_key(&mut self, ctx: &mut EventContext, key: &Key) {
        let mods = key.mods;
        let empty = Modifiers::empty();
        let ctrl = Modifiers::CTRL;
        let alt = Modifiers::ALT;

        // Autocomplete popup intercept, ahead of the jump-mode and editing
        // chains. While the popup is open its navigation / accept / cancel keys
        // are consumed here; a printable char falls through to the normal insert
        // path, which re-triggers and narrows the popup.
        if self.autocomplete_state.is_some() {
            if key.matches(Key::ESCAPE, empty) {
                self.cancel_autocomplete();
                ctx.consume_and_redraw();
                return;
            }
            let up = key.matches(Key::UP, empty) || key.matches(u32::from('p'), ctrl);
            let down = key.matches(Key::DOWN, empty) || key.matches(u32::from('n'), ctrl);
            if (up || down) && !self.autocomplete_items.is_empty() {
                self.move_autocomplete_selection(down);
                ctx.consume_and_redraw();
                return;
            }
            if key.matches(Key::TAB, empty) {
                // Tab applies the current selection without re-querying.
                self.apply_selected_autocomplete();
                self.check_changed(ctx);
                return;
            }
            if key.matches(Key::ENTER, empty) {
                self.accept_autocomplete_on_enter();
                self.check_changed(ctx);
                return;
            }
            // Any other key (printable, motion, deletion) falls through.
        }

        // Tab with the popup closed: force a completion request.
        if self.autocomplete_provider.is_some()
            && self.autocomplete_state.is_none()
            && key.matches(Key::TAB, empty)
        {
            self.trigger_force_autocomplete();
            ctx.consume_and_redraw();
            return;
        }

        // Char-jump mode. Intercepted before every editing chord so the next
        // key is read as a jump target rather than an edit. The three-way
        // intercept: (1) re-pressing the same-direction chord cancels, (2) a
        // printable char with only Shift held is the jump target, (3) anything
        // else clears the mode and falls through so the key does its normal job
        // (Esc must still reach the parent surface).
        if let Some(direction) = self.jump_mode {
            let cancels = match direction {
                JumpDirection::Forward => key.matches(u32::from(']'), ctrl),
                JumpDirection::Backward => key.matches(u32::from(']'), ctrl | alt),
            };
            if cancels {
                self.jump_mode = None;
                ctx.consume_event();
                return;
            }
            if (mods - Modifiers::SHIFT).is_empty()
                && let Some(needle) = printable_char(key)
            {
                self.jump_to_char(needle, direction);
                self.jump_mode = None;
                self.check_changed(ctx);
                return;
            }
            self.jump_mode = None;
        }

        // Enter char-jump mode. Placed after the active-mode intercept so that
        // pressing the other direction while in one mode (a non-cancel, non-jump
        // key) clears the current mode above and then switches here.
        if key.matches(u32::from(']'), ctrl) {
            self.jump_mode = Some(JumpDirection::Forward);
            ctx.consume_event();
            return;
        }
        if key.matches(u32::from(']'), ctrl | alt) {
            self.jump_mode = Some(JumpDirection::Backward);
            ctx.consume_event();
            return;
        }

        // Undo.
        if key.matches(u32::from('_'), ctrl)
            || key.matches(u32::from('-'), ctrl)
            || key.matches(u32::from('/'), ctrl)
            || key.matches(u32::from('z'), ctrl)
        {
            self.restore_undo();
            self.check_changed(ctx);
            return;
        }

        // Newline, checked before submit so Shift+Enter and friends never
        // submit.
        let is_newline = key.matches(Key::ENTER, Modifiers::SHIFT)
            || key.matches(Key::ENTER, alt)
            || key.matches(u32::from('j'), ctrl)
            || key.matches(0x0A, empty);
        if is_newline {
            self.save_undo();
            self.insert_newline_internal();
            self.reset_sticky_state();
            self.last_action = LastAction::None;
            self.check_changed(ctx);
            return;
        }

        // Submit (plain Enter). A `\` immediately before the cursor inserts a
        // newline instead, the standard multi-line escape hatch.
        if key.matches(Key::ENTER, empty) {
            if !self.submit_enabled {
                ctx.consume_event();
                return;
            }
            if self.cursor_preceded_by_backslash() {
                self.save_undo();
                // This is the only direct `self.lines` edit outside the
                // dedicated editing methods, so it invalidates the cache here
                // to keep the "every mutation invalidates" invariant local.
                // `insert_newline_internal` below also invalidates, which is
                // harmless.
                self.lines[self.cursor_line].remove(self.cursor_col - 1);
                self.invalidate_visual_line_cache();
                self.cursor_col -= 1;
                self.insert_newline_internal();
                self.reset_sticky_state();
                self.last_action = LastAction::None;
                self.check_changed(ctx);
                return;
            }
            self.submit_value(ctx);
            self.check_changed(ctx);
            return;
        }

        // Vertical movement (history-aware).
        if key.matches(Key::UP, empty) || key.matches(u32::from('p'), ctrl) {
            self.on_cursor_up();
            self.check_changed(ctx);
            return;
        }
        if key.matches(Key::DOWN, empty) || key.matches(u32::from('n'), ctrl) {
            self.on_cursor_down();
            self.check_changed(ctx);
            return;
        }

        // Page up/down.
        if key.matches(Key::PAGE_UP, empty) {
            self.page_scroll(-1);
            self.check_changed(ctx);
            return;
        }
        if key.matches(Key::PAGE_DOWN, empty) {
            self.page_scroll(1);
            self.check_changed(ctx);
            return;
        }

        // Word movement, before plain movement.
        if key.matches(u32::from('b'), alt)
            || key.matches(Key::LEFT, alt)
            || key.matches(Key::LEFT, ctrl)
        {
            self.move_word_left();
            self.reset_sticky_state();
            self.last_action = LastAction::None;
            self.check_changed(ctx);
            return;
        }
        if key.matches(u32::from('f'), alt)
            || key.matches(Key::RIGHT, alt)
            || key.matches(Key::RIGHT, ctrl)
        {
            self.move_word_right();
            self.reset_sticky_state();
            self.last_action = LastAction::None;
            self.check_changed(ctx);
            return;
        }

        // Plain horizontal movement.
        if key.matches(Key::LEFT, empty) || key.matches(u32::from('b'), ctrl) {
            self.move_left();
            self.last_action = LastAction::None;
            self.check_changed(ctx);
            return;
        }
        if key.matches(Key::RIGHT, empty) || key.matches(u32::from('f'), ctrl) {
            self.move_right();
            self.last_action = LastAction::None;
            self.check_changed(ctx);
            return;
        }
        if key.matches(u32::from('a'), ctrl) || key.matches(Key::HOME, empty) {
            self.cursor_col = 0;
            self.reset_sticky_state();
            self.last_action = LastAction::None;
            self.check_changed(ctx);
            return;
        }
        if key.matches(u32::from('e'), ctrl) || key.matches(Key::END, empty) {
            self.cursor_col = self.current_line().len();
            self.reset_sticky_state();
            self.last_action = LastAction::None;
            self.check_changed(ctx);
            return;
        }

        // Deletion, word-level before char-level.
        if key.matches(u32::from('w'), ctrl) || key.matches(Key::BACKSPACE, alt) {
            self.kill_word_backward();
            self.check_changed(ctx);
            return;
        }
        if key.matches(u32::from('d'), alt) {
            self.kill_word_forward();
            self.check_changed(ctx);
            return;
        }
        if key.matches(Key::BACKSPACE, empty) {
            self.backspace();
            self.check_changed(ctx);
            return;
        }
        if key.matches(Key::DELETE, empty) || key.matches(u32::from('d'), ctrl) {
            self.delete_forward();
            self.check_changed(ctx);
            return;
        }
        if key.matches(u32::from('u'), ctrl) {
            self.kill_to_start();
            self.check_changed(ctx);
            return;
        }
        if key.matches(u32::from('k'), ctrl) {
            self.kill_to_end();
            self.check_changed(ctx);
            return;
        }

        // Yank.
        if key.matches(u32::from('y'), ctrl) {
            self.yank();
            self.check_changed(ctx);
            return;
        }
        if key.matches(u32::from('y'), alt) {
            self.yank_pop();
            self.check_changed(ctx);
            return;
        }

        // Character insertion. Only printable text with no modifier beyond
        // Shift inserts, so Ctrl/Alt combos stay bindings.
        if (mods - Modifiers::SHIFT).is_empty()
            && let Some(text) = key.text.as_deref()
        {
            // Palette trigger: a plain `/` at an empty start-of-message fires
            // the callback and swallows the `/`.
            if text == "/" && self.on_palette_trigger.is_some() && self.is_at_start_of_message() {
                if let Some(cb) = self.on_palette_trigger.as_mut() {
                    cb(ctx);
                }
                ctx.consume_and_redraw();
                return;
            }
            let mut inserted = false;
            for c in text.chars() {
                if !c.is_control() {
                    self.insert_char(c);
                    inserted = true;
                }
            }
            if inserted {
                self.check_changed(ctx);
            }
        }
    }
}

/// Whether `seg` counts as whitespace for a wrap opportunity: non-empty and
/// every scalar is whitespace. A multi-scalar atom is whitespace only when all
/// of it is, so an atom carrying both a space and a non-space is not.
fn is_segment_whitespace(seg: &str) -> bool {
    !seg.is_empty() && seg.chars().all(char::is_whitespace)
}

/// The single printable character a key would insert, if any.
///
/// Char-jump mode reads its search target with this. A key qualifies when its
/// text is exactly one non-control character. Multi-character text (a paste, a
/// composed sequence) and control keys (Esc, arrows) return `None` so they fall
/// through to the mode's silent-cancel path.
fn printable_char(key: &Key) -> Option<char> {
    let text = key.text.as_deref()?;
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() || c.is_control() {
        return None;
    }
    Some(c)
}

/// The fixed editing chords. See [`TextArea::bindings`].
static BINDINGS: [ChordDoc; 25] = [
    ChordDoc {
        keys: "↑ / Ctrl-P",
        description: "Move up, or previous history at the top line",
        group: "Movement",
    },
    ChordDoc {
        keys: "↓ / Ctrl-N",
        description: "Move down, or next history at the bottom line",
        group: "Movement",
    },
    ChordDoc {
        keys: "← / Ctrl-B",
        description: "Move left",
        group: "Movement",
    },
    ChordDoc {
        keys: "→ / Ctrl-F",
        description: "Move right",
        group: "Movement",
    },
    ChordDoc {
        keys: "Alt-B / Ctrl-←",
        description: "Move one word left",
        group: "Movement",
    },
    ChordDoc {
        keys: "Alt-F / Ctrl-→",
        description: "Move one word right",
        group: "Movement",
    },
    ChordDoc {
        keys: "Ctrl-A / Home",
        description: "Move to line start",
        group: "Movement",
    },
    ChordDoc {
        keys: "Ctrl-E / End",
        description: "Move to line end",
        group: "Movement",
    },
    ChordDoc {
        keys: "PageUp",
        description: "Scroll up one page",
        group: "Movement",
    },
    ChordDoc {
        keys: "PageDown",
        description: "Scroll down one page",
        group: "Movement",
    },
    ChordDoc {
        keys: "Backspace",
        description: "Delete the character before the cursor",
        group: "Editing",
    },
    ChordDoc {
        keys: "Ctrl-D / Delete",
        description: "Delete the character after the cursor",
        group: "Editing",
    },
    ChordDoc {
        keys: "Ctrl-W / Alt-Backspace",
        description: "Delete the word before the cursor",
        group: "Editing",
    },
    ChordDoc {
        keys: "Alt-D",
        description: "Delete the word after the cursor",
        group: "Editing",
    },
    ChordDoc {
        keys: "Ctrl-U",
        description: "Kill to line start",
        group: "Kill ring",
    },
    ChordDoc {
        keys: "Ctrl-K",
        description: "Kill to line end",
        group: "Kill ring",
    },
    ChordDoc {
        keys: "Ctrl-Y",
        description: "Yank the last kill",
        group: "Kill ring",
    },
    ChordDoc {
        keys: "Alt-Y",
        description: "Cycle the kill ring (yank-pop)",
        group: "Kill ring",
    },
    ChordDoc {
        keys: "Ctrl-_ / Ctrl-Z",
        description: "Undo",
        group: "Editing",
    },
    ChordDoc {
        keys: "Shift-Enter / Alt-Enter / Ctrl-J",
        description: "Insert a newline",
        group: "Editing",
    },
    ChordDoc {
        keys: "Enter",
        description: "Submit",
        group: "Editing",
    },
    ChordDoc {
        keys: "Ctrl-]",
        description: "Jump to the next occurrence of a character",
        group: "Movement",
    },
    ChordDoc {
        keys: "Ctrl-Alt-]",
        description: "Jump to the previous occurrence of a character",
        group: "Movement",
    },
    ChordDoc {
        keys: "Tab",
        description: "Complete: apply a suggestion, or open the popup",
        group: "Autocomplete",
    },
    ChordDoc {
        keys: "Esc",
        description: "Dismiss the autocomplete popup (when open)",
        group: "Autocomplete",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    // The per-frame matcher budget must stay small: it runs on the UI thread
    // before every render while a streaming session is open, so a large budget
    // reintroduces the keystroke latency this const exists to prevent. Nucleo
    // converges over multiple notify-driven frames, so a couple of milliseconds
    // per frame is enough to keep the popup populating.
    #[test]
    fn autocomplete_tick_budget_stays_small() {
        assert!(
            AUTOCOMPLETE_TICK_BUDGET_MS <= 5,
            "tick budget {AUTOCOMPLETE_TICK_BUDGET_MS}ms is too large to keep typing snappy",
        );
    }

    // -- Test harness --

    /// Builds a bare, focused-agnostic editor for driving directly.
    fn editor() -> TextArea {
        TextArea::new_state()
    }

    /// A plain `char` key press with matching `text`.
    fn char_key(c: char) -> Event {
        Event::KeyPress(Key {
            codepoint: u32::from(c),
            text: Some(c.to_string().into()),
            ..Key::default()
        })
    }

    /// A special-key press by codepoint and modifiers, with no text.
    fn key(codepoint: u32, mods: Modifiers) -> Event {
        Event::KeyPress(Key {
            codepoint,
            mods,
            ..Key::default()
        })
    }

    /// A modified letter press (e.g. Alt-B), carrying the letter codepoint.
    fn mod_key(c: char, mods: Modifiers) -> Event {
        Event::KeyPress(Key {
            codepoint: u32::from(c),
            mods,
            ..Key::default()
        })
    }

    fn send(ed: &mut TextArea, event: &Event) {
        let mut ctx = EventContext::new();
        ed.handle_event(&mut ctx, event);
    }

    /// Sends `event` and reports whether the widget consumed it. Used by the
    /// jump-mode tests, whose fall-through assertions turn on consumption.
    fn send_consumed(ed: &mut TextArea, event: &Event) -> bool {
        let mut ctx = EventContext::new();
        ed.handle_event(&mut ctx, event);
        ctx.consume_event
    }

    /// A bracketed-paste event carrying `text`.
    fn paste(text: &str) -> Event {
        Event::Paste(text.to_string())
    }

    /// The forward and backward char-jump entry chords.
    fn jump_forward() -> Event {
        mod_key(']', Modifiers::CTRL)
    }
    fn jump_backward() -> Event {
        mod_key(']', Modifiers::CTRL | Modifiers::ALT)
    }

    /// Hand-scans the single paste marker in `text`, returning its byte length
    /// and the matched token. Scans for the prefix and the next closing `]`, so
    /// no regex test-dependency is needed.
    fn find_marker(text: &str) -> (usize, String) {
        let start = text.find("[paste #").expect("marker prefix present");
        let close = text[start..].find(']').expect("marker close present");
        let marker = text[start..start + close + 1].to_string();
        (marker.len(), marker)
    }

    /// Pastes 20 single-word lines, which crosses the `>10 lines` threshold and
    /// produces a `+N lines` marker. Returns the resulting displayed text.
    fn paste_large(ed: &mut TextArea) -> String {
        let big = "line\n".repeat(20).trim_end().to_string();
        send(ed, &paste(&big));
        ed.text()
    }

    /// Pastes `n_lines` single-word lines, crossing the threshold once
    /// `n_lines > 10`.
    fn paste_n_lines(ed: &mut TextArea, n_lines: usize) {
        let content = "line\n".repeat(n_lines).trim_end().to_string();
        send(ed, &paste(&content));
    }

    /// Pastes `n_chars` literal `x` characters, producing a `N chars` marker
    /// once past the 1000-char threshold.
    fn paste_n_chars(ed: &mut TextArea, n_chars: usize) {
        send(ed, &paste(&"x".repeat(n_chars)));
    }

    // Short key-event constructors, named after the chords they stand for, to
    // keep the ported test bodies readable.
    fn right() -> Event {
        key(Key::RIGHT, Modifiers::empty())
    }
    fn left() -> Event {
        key(Key::LEFT, Modifiers::empty())
    }
    fn up() -> Event {
        key(Key::UP, Modifiers::empty())
    }
    fn down() -> Event {
        key(Key::DOWN, Modifiers::empty())
    }
    fn ctrl_right() -> Event {
        key(Key::RIGHT, Modifiers::CTRL)
    }
    fn backspace() -> Event {
        key(Key::BACKSPACE, Modifiers::empty())
    }
    fn delete() -> Event {
        key(Key::DELETE, Modifiers::empty())
    }
    fn escape() -> Event {
        key(Key::ESCAPE, Modifiers::empty())
    }
    fn shift_enter() -> Event {
        key(Key::ENTER, Modifiers::SHIFT)
    }
    fn ctrl(c: char) -> Event {
        mod_key(c, Modifiers::CTRL)
    }

    /// Asserts every visual line of the document fits the layout width, the
    /// property the wide-atom sub-split must maintain. A lone grapheme wider
    /// than the width is the only permitted overflow.
    fn assert_all_vls_fit(ed: &TextArea) {
        let vls = ed.compute_visual_line_map(ed.layout_width);
        for vl in &vls {
            let line = &ed.lines[vl.logical_line];
            let span = &line[vl.start_col..vl.start_col + vl.length];
            let w = ed.measure(span);
            assert!(
                w <= ed.layout_width || span.graphemes(true).count() <= 1,
                "visual line {span:?} width {w} exceeds layout width {}",
                ed.layout_width,
            );
        }
    }

    fn type_str(ed: &mut TextArea, s: &str) {
        for c in s.chars() {
            send(ed, &char_key(c));
        }
    }

    fn ctx(w: u16, h: u16) -> DrawContext {
        DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: crate::vxfw::MaxSize {
                width: Some(w),
                height: Some(h),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        }
    }

    /// Concatenates the grapheme of every cell in `row` of `surf`.
    fn row_text(surf: &Surface, row: u16) -> String {
        let mut out = String::new();
        for col in 0..surf.size.width {
            out.push_str(surf.read_cell(col, row).char.grapheme());
        }
        out
    }

    // -- The required per-module doctest --

    #[test]
    fn text_area() {
        let mut ed = editor();
        type_str(&mut ed, "hi");
        assert_eq!(ed.text(), "hi");
        assert_eq!(ed.cursor(), (0, 2));

        send(&mut ed, &key(Key::BACKSPACE, Modifiers::empty()));
        assert_eq!(ed.text(), "h");

        // Draws a bordered surface: top rule, one content row, bottom rule.
        let surf = ed.draw(&ctx(20, 10));
        assert_eq!(surf.size.height, 3);
        assert!(surf.cursor.is_some());
    }

    // -- Visual-line cache --

    /// The visual-line cache's only failure mode is staleness: an edit path
    /// that mutates `lines` without invalidating would serve a map that no
    /// longer matches the document. After every representative edit we assert
    /// the cached map equals a freshly computed one. Because `visual_line_map`
    /// returns the cache unchanged when the width matches, a missing
    /// invalidation surfaces here as a stale hit that differs from
    /// `compute_visual_line_map`. This test fails if any edit path forgets to
    /// invalidate.
    #[test]
    fn visual_line_cache_never_serves_stale_map() {
        // Navigation primes the cache at `layout_width`, so pin `layout_width`
        // to the width we assert at. Otherwise a resize-driven rebuild would
        // mask a missing invalidation.
        let w = 12usize;
        let mut ed = editor();
        ed.layout_width = w;

        // Asserts the cached map matches a fresh build, then leaves the cache
        // primed at `w` for the next edit.
        fn check(ed: &mut TextArea, w: usize, label: &str) {
            let fresh = ed.compute_visual_line_map(w);
            let cached = ed.visual_line_map(w).to_vec();
            assert_eq!(cached, fresh, "stale visual-line cache after {label}");
        }

        // Prime before the first edit so a missing invalidation is a stale hit,
        // not a cold miss.
        ed.visual_line_map(w);

        type_str(&mut ed, "hello world foo bar baz");
        check(&mut ed, w, "insert chars");

        send(&mut ed, &shift_enter());
        check(&mut ed, w, "insert newline");

        send(&mut ed, &backspace());
        check(&mut ed, w, "backspace");

        type_str(&mut ed, "abc");
        send(&mut ed, &left());
        send(&mut ed, &delete());
        check(&mut ed, w, "delete forward");

        paste_n_lines(&mut ed, 20);
        check(&mut ed, w, "large paste (marker collapse)");

        send(&mut ed, &ctrl('a'));
        send(&mut ed, &ctrl('k'));
        check(&mut ed, w, "kill to end of line");

        send(&mut ed, &ctrl('y'));
        check(&mut ed, w, "yank");

        send(&mut ed, &ctrl('z'));
        check(&mut ed, w, "undo");

        ed.set_text("a\nbb\nccc\ndddd eeee ffff gggg hhhh");
        check(&mut ed, w, "set_text");

        // History up then down each swap the whole document through
        // `set_document`.
        ed.clear();
        check(&mut ed, w, "clear");
        ed.add_to_history("history entry one two three four");
        send(&mut ed, &up());
        check(&mut ed, w, "history up");
        send(&mut ed, &down());
        check(&mut ed, w, "history down");

        // A width change must rebuild even with no edit: the accessor keys on
        // width. Use a document that wraps differently at the two widths so the
        // rebuilt map is observably different.
        ed.set_text("dddd eeee ffff gggg hhhh iiii jjjj");
        let narrow = ed.visual_line_map(w).to_vec();
        let wide = ed.visual_line_map(40).to_vec();
        assert_eq!(
            wide,
            ed.compute_visual_line_map(40),
            "resize did not rebuild the map"
        );
        assert_ne!(narrow, wide, "resize should produce a different map");
    }

    // -- Insert / delete --

    #[test]
    fn insert_and_delete_basics() {
        let mut ed = editor();
        type_str(&mut ed, "hello");
        assert_eq!(ed.text(), "hello");
        send(&mut ed, &key(Key::BACKSPACE, Modifiers::empty()));
        assert_eq!(ed.text(), "hell");
        // Ctrl-A to start, delete-forward removes 'h'.
        send(&mut ed, &mod_key('a', Modifiers::CTRL));
        send(&mut ed, &key(Key::DELETE, Modifiers::empty()));
        assert_eq!(ed.text(), "ell");
    }

    #[test]
    fn newline_via_shift_enter() {
        let mut ed = editor();
        type_str(&mut ed, "a");
        send(&mut ed, &key(Key::ENTER, Modifiers::SHIFT));
        type_str(&mut ed, "b");
        assert_eq!(ed.text(), "a\nb");
        assert_eq!(ed.cursor(), (1, 1));
    }

    // -- Word motion (three-class emacs feel) --

    #[test]
    fn word_motion_stops_on_punctuation_runs() {
        // EmacsWords: `...` is its own word between `bar` and `baz`.
        let mut ed = editor();
        ed.set_text("foo bar... baz");
        ed.cursor_col = 0;
        send(&mut ed, &mod_key('f', Modifiers::ALT));
        assert_eq!(ed.cursor_col, 3, "end of foo");
        send(&mut ed, &mod_key('f', Modifiers::ALT));
        assert_eq!(ed.cursor_col, 7, "end of bar");
        send(&mut ed, &mod_key('f', Modifiers::ALT));
        assert_eq!(ed.cursor_col, 10, "end of ...");
        send(&mut ed, &mod_key('f', Modifiers::ALT));
        assert_eq!(ed.cursor_col, 14, "end of baz");
    }

    #[test]
    fn word_motion_left_lands_before_punctuation_run() {
        let mut ed = editor();
        ed.set_text("foo bar...");
        // Cursor at end.
        send(&mut ed, &mod_key('b', Modifiers::ALT));
        assert_eq!(ed.cursor_col, 7, "before ...");
        send(&mut ed, &mod_key('b', Modifiers::ALT));
        assert_eq!(ed.cursor_col, 4, "before bar");
        send(&mut ed, &mod_key('b', Modifiers::ALT));
        assert_eq!(ed.cursor_col, 0, "start");
    }

    #[test]
    fn word_motion_wraps_across_lines() {
        let mut ed = editor();
        ed.set_text("foo\nbar");
        ed.cursor_line = 1;
        ed.cursor_col = 0;
        send(&mut ed, &mod_key('b', Modifiers::ALT));
        assert_eq!((ed.cursor_line, ed.cursor_col), (0, 3));
    }

    // -- Vertical movement with sticky column --

    #[test]
    fn vertical_move_sticky_column_over_short_line() {
        // "hello world"(11) / "hi"(2) / "foobar"(6). Down from col 8 clamps to
        // the short line's end while remembering 8, then re-expands.
        let mut ed = editor();
        ed.set_text("hello world\nhi\nfoobar");
        ed.cursor_line = 0;
        ed.cursor_col = 8;

        ed.move_down();
        assert_eq!((ed.cursor_line, ed.cursor_col), (1, 2));
        ed.move_down();
        assert_eq!((ed.cursor_line, ed.cursor_col), (2, 6));
        ed.move_up();
        assert_eq!((ed.cursor_line, ed.cursor_col), (1, 2));
        ed.move_up();
        // Sticky column restores column 8 on the long line.
        assert_eq!((ed.cursor_line, ed.cursor_col), (0, 8));
    }

    #[test]
    fn vertical_move_preferred_column_survives_middle_line() {
        // A short middle line does not clobber the preferred column.
        let mut ed = editor();
        ed.set_text("abcdefgh\nxy\nabcdefgh");
        ed.cursor_line = 0;
        ed.cursor_col = 5;
        ed.move_down();
        assert_eq!((ed.cursor_line, ed.cursor_col), (1, 2));
        ed.move_down();
        assert_eq!((ed.cursor_line, ed.cursor_col), (2, 5));
    }

    #[test]
    fn vertical_move_target_fits_keeps_column() {
        // Case 1 of the table: starting nav onto a longer line keeps the column.
        let mut ed = editor();
        ed.set_text("abcd\nabcdefgh");
        ed.cursor_line = 0;
        ed.cursor_col = 2;
        ed.move_down();
        assert_eq!((ed.cursor_line, ed.cursor_col), (1, 2));
    }

    #[test]
    fn vertical_move_across_wrapped_visual_lines() {
        // "hello world foo" wraps at width 11 into "hello " and "world foo".
        let mut ed = editor();
        ed.set_text("hello world foo");
        ed.layout_width = 11;
        ed.width_method = gwidth::Method::Unicode;
        ed.cursor_line = 0;
        ed.cursor_col = 2;
        ed.move_down();
        // Column 2 of the second visual row is byte 8 ('r' of "world").
        assert_eq!((ed.cursor_line, ed.cursor_col), (0, 8));
        ed.move_up();
        assert_eq!((ed.cursor_line, ed.cursor_col), (0, 2));
    }

    #[test]
    fn vertical_move_snaps_onto_multibyte_grapheme_and_round_trips() {
        // Moving down from an ASCII byte column that lands strictly inside a
        // multi-byte grapheme snaps the cursor to the segment start and records
        // the pre-snap column, so moving back up restores the original column.
        let mut ed = editor();
        // '中' occupies bytes [1, 4); byte column 3 is strictly inside it.
        ed.set_text("abcd\nx中y");
        ed.cursor_line = 0;
        ed.cursor_col = 3;

        ed.move_down();
        // Snapped to the start of '中' (byte 1), not left mid-grapheme.
        assert_eq!((ed.cursor_line, ed.cursor_col), (1, 1));
        assert_eq!(ed.snapped_from_cursor_col, Some(3));

        ed.move_up();
        // The pre-snap anchor restores byte column 3 on the ASCII line.
        assert_eq!((ed.cursor_line, ed.cursor_col), (0, 3));
        assert_eq!(ed.snapped_from_cursor_col, None);
    }

    // -- Wrapping --

    #[test]
    fn wrap_reconstructs_line_exactly() {
        let ed = editor();
        let line = "some long text with several words to wrap";
        let spans = ed.wrap_line_spans(line, 10);
        let joined: String = spans.iter().map(|&(s, e)| &line[s..e]).collect();
        assert_eq!(joined, line);
        for &(s, e) in &spans {
            assert!(s <= e && e <= line.len());
        }
    }

    #[test]
    fn wrap_breaks_at_word_opportunity() {
        let ed = editor();
        let spans = ed.wrap_line_spans("hello world foo", 11);
        assert_eq!(spans, vec![(0, 6), (6, 15)]);
    }

    #[test]
    fn wrap_force_breaks_a_long_word() {
        let ed = editor();
        // No whitespace, so every overflow force-breaks at the width.
        let spans = ed.wrap_line_spans("abcdefghijklmnop", 5);
        assert_eq!(spans, vec![(0, 5), (5, 10), (10, 15), (15, 16)]);
    }

    #[test]
    fn wrap_keeps_oversized_grapheme_whole() {
        let ed = editor();
        // Each CJK char is width 2 and 3 bytes; at width 1 each stays whole.
        let line = "中中";
        let spans = ed.wrap_line_spans(line, 1);
        assert_eq!(spans, vec![(0, 3), (3, 6)]);
        let joined: String = spans.iter().map(|&(s, e)| &line[s..e]).collect();
        assert_eq!(joined, line);
    }

    #[test]
    fn wrap_reconstructs_multi_and_trailing_spaces() {
        // Runs of spaces and a trailing space must survive wrapping so the
        // visual-line map stays byte-synced with the document navigation reads.
        let ed = editor();
        let line = "a  bb   c  ";
        for width in 1..=12 {
            let spans = ed.wrap_line_spans(line, width);
            let joined: String = spans.iter().map(|&(s, e)| &line[s..e]).collect();
            assert_eq!(joined, line, "width {width} must reconstruct the line");
            let mut prev_end = 0;
            for &(s, e) in &spans {
                assert_eq!(s, prev_end, "spans must be contiguous at width {width}");
                assert!(s <= e && e <= line.len());
                prev_end = e;
            }
            assert_eq!(prev_end, line.len(), "spans must cover the line");
        }
    }

    // -- Kill ring --

    #[test]
    fn kill_to_end_and_yank() {
        let mut ed = editor();
        ed.set_text("hello world");
        ed.cursor_col = 5;
        send(&mut ed, &mod_key('k', Modifiers::CTRL));
        assert_eq!(ed.text(), "hello");
        assert_eq!(ed.kill_ring.peek(), Some(" world"));
        send(&mut ed, &mod_key('y', Modifiers::CTRL));
        assert_eq!(ed.text(), "hello world");
    }

    #[test]
    fn kill_word_backward_accumulates_with_prepend() {
        let mut ed = editor();
        ed.set_text("ab cd");
        // Cursor at end. Two backward word kills accumulate in prepend order.
        send(&mut ed, &mod_key('w', Modifiers::CTRL));
        assert_eq!(ed.text(), "ab ");
        assert_eq!(ed.kill_ring.peek(), Some("cd"));
        send(&mut ed, &mod_key('w', Modifiers::CTRL));
        assert_eq!(ed.text(), "");
        assert_eq!(ed.kill_ring.peek(), Some("ab cd"));
    }

    #[test]
    fn yank_pop_rotates_the_ring() {
        let mut ed = editor();
        ed.set_text("hello world");
        ed.cursor_col = 5;
        // First kill: " world".
        ed.kill_to_end();
        // Break the kill chain, then kill "hello" as a separate entry.
        ed.last_action = LastAction::None;
        ed.cursor_col = 0;
        ed.kill_to_end();
        assert_eq!(ed.text(), "");
        // Yank the most recent ("hello"), then yank-pop to the older (" world").
        send(&mut ed, &mod_key('y', Modifiers::CTRL));
        assert_eq!(ed.text(), "hello");
        send(&mut ed, &mod_key('y', Modifiers::ALT));
        assert_eq!(ed.text(), " world");
    }

    // -- Undo coalescing --

    #[test]
    fn undo_coalesces_words_not_whitespace() {
        // Typing "hello world" pushes two snapshots: before 'h' and before ' '.
        let mut ed = editor();
        type_str(&mut ed, "hello world");
        assert_eq!(ed.text(), "hello world");
        send(&mut ed, &mod_key('z', Modifiers::CTRL));
        assert_eq!(ed.text(), "hello");
        send(&mut ed, &mod_key('z', Modifiers::CTRL));
        assert_eq!(ed.text(), "");
    }

    #[test]
    fn undo_each_space_separately() {
        // "hello  " (two spaces) pushes three snapshots.
        let mut ed = editor();
        type_str(&mut ed, "hello  ");
        assert_eq!(ed.text(), "hello  ");
        send(&mut ed, &mod_key('z', Modifiers::CTRL));
        assert_eq!(ed.text(), "hello ");
        send(&mut ed, &mod_key('z', Modifiers::CTRL));
        assert_eq!(ed.text(), "hello");
        send(&mut ed, &mod_key('z', Modifiers::CTRL));
        assert_eq!(ed.text(), "");
    }

    // -- History --

    #[test]
    fn history_draft_preserved_and_restored() {
        // The history_up method parks the in-progress buffer as a draft and
        // history_down restores it.
        let mut ed = editor();
        ed.add_to_history("first");
        ed.add_to_history("second");
        ed.set_document("draft");

        ed.history_up();
        assert_eq!(ed.text(), "second");
        ed.history_up();
        assert_eq!(ed.text(), "first");
        ed.history_down();
        assert_eq!(ed.text(), "second");
        ed.history_down();
        assert_eq!(ed.text(), "draft");
    }

    #[test]
    fn history_up_on_empty_prompt_walks_newest_first() {
        let mut ed = editor();
        ed.seed_history(&[
            "oldest".to_string(),
            "middle".to_string(),
            "newest".to_string(),
        ]);
        send(&mut ed, &key(Key::UP, Modifiers::empty()));
        assert_eq!(ed.text(), "newest");
        send(&mut ed, &key(Key::UP, Modifiers::empty()));
        assert_eq!(ed.text(), "middle");
        send(&mut ed, &key(Key::UP, Modifiers::empty()));
        assert_eq!(ed.text(), "oldest");
    }

    #[test]
    fn up_on_nonempty_top_line_jumps_to_start_not_history() {
        // With typed content, Up moves the cursor rather than entering history.
        let mut ed = editor();
        ed.add_to_history("prev");
        type_str(&mut ed, "typed");
        send(&mut ed, &key(Key::UP, Modifiers::empty()));
        assert_eq!(ed.text(), "typed");
        assert_eq!(ed.cursor(), (0, 0));
    }

    // -- Submit / newline / backslash --

    #[test]
    fn submit_fires_callback_and_clears() {
        let seen = Rc::new(RefCell::new(String::new()));
        let mut ed = editor();
        let sink = Rc::clone(&seen);
        ed.on_submit = Some(Box::new(move |_ctx, s| {
            *sink.borrow_mut() = s.to_string();
        }));
        type_str(&mut ed, "  hi there  ");
        send(&mut ed, &key(Key::ENTER, Modifiers::empty()));
        assert_eq!(seen.borrow().as_str(), "hi there");
        assert_eq!(ed.text(), "");
        assert_eq!(ed.take_submitted().as_deref(), Some("hi there"));
    }

    #[test]
    fn submit_disabled_is_silent() {
        let mut ed = editor();
        ed.set_submit_enabled(false);
        type_str(&mut ed, "keep");
        send(&mut ed, &key(Key::ENTER, Modifiers::empty()));
        assert_eq!(ed.text(), "keep");
    }

    #[test]
    fn backslash_enter_inserts_newline() {
        let mut ed = editor();
        type_str(&mut ed, "a\\");
        send(&mut ed, &key(Key::ENTER, Modifiers::empty()));
        assert_eq!(ed.text(), "a\n");
        assert_eq!(ed.take_submitted(), None);
    }

    // -- Palette trigger --

    #[test]
    fn palette_trigger_fires_on_slash_at_empty_start() {
        let fired = Rc::new(RefCell::new(false));
        let mut ed = editor();
        let flag = Rc::clone(&fired);
        ed.on_palette_trigger = Some(Box::new(move |_ctx| {
            *flag.borrow_mut() = true;
        }));
        send(&mut ed, &char_key('/'));
        assert!(*fired.borrow());
        assert_eq!(ed.text(), "");
    }

    #[test]
    fn slash_inserts_without_palette_trigger() {
        let mut ed = editor();
        send(&mut ed, &char_key('/'));
        assert_eq!(ed.text(), "/");
    }

    #[test]
    fn palette_trigger_ignores_mid_line_slash() {
        let fired = Rc::new(RefCell::new(false));
        let mut ed = editor();
        let flag = Rc::clone(&fired);
        ed.on_palette_trigger = Some(Box::new(move |_ctx| {
            *flag.borrow_mut() = true;
        }));
        type_str(&mut ed, "ab");
        send(&mut ed, &char_key('/'));
        assert!(!*fired.borrow());
        assert_eq!(ed.text(), "ab/");
    }

    // -- on_change --

    #[test]
    fn on_change_fires_only_on_actual_edits() {
        let seen = Rc::new(RefCell::new(String::new()));
        let mut ed = editor();
        let sink = Rc::clone(&seen);
        ed.on_change = Some(Box::new(move |_ctx, s| {
            *sink.borrow_mut() = s.to_string();
        }));
        type_str(&mut ed, "hi");
        assert_eq!(seen.borrow().as_str(), "hi");
        // A cursor-only move does not change the text.
        *seen.borrow_mut() = "sentinel".to_string();
        send(&mut ed, &mod_key('a', Modifiers::CTRL));
        assert_eq!(seen.borrow().as_str(), "sentinel");
    }

    // -- Rendering --

    #[test]
    fn top_bar_label_inlaid_in_top_rule() {
        let mut ed = editor();
        ed.set_top_bar_label(Some("agent 2".to_string()));
        let surf = ed.draw(&ctx(40, 10));
        assert!(row_text(&surf, 0).contains("agent 2"));
    }

    #[test]
    fn no_top_bar_label_is_a_plain_rule() {
        let mut ed = editor();
        let surf = ed.draw(&ctx(20, 10));
        assert_eq!(row_text(&surf, 0), "─".repeat(20));
    }

    #[test]
    fn scroll_indicator_appears_when_scrolled() {
        let mut ed = editor();
        ed.set_max_visible_rows(Some(2));
        ed.set_text("l1\nl2\nl3\nl4\nl5");
        let surf = ed.draw(&ctx(40, 10));
        let top = row_text(&surf, 0);
        assert!(top.contains('↑'), "top rule: {top:?}");
        assert!(top.contains("more"), "top rule: {top:?}");
        // Two content rows plus two rules.
        assert_eq!(surf.size.height, 4);
    }

    #[test]
    fn cursor_is_reported_at_expected_cell() {
        let mut ed = editor();
        type_str(&mut ed, "abc");
        let surf = ed.draw(&ctx(20, 10));
        let cursor = surf.cursor.expect("cursor reported");
        // Row 1 (below the top rule), column 3 (after "abc"), no padding.
        assert_eq!((cursor.row, cursor.col), (1, 3));
    }

    #[test]
    fn padding_shifts_content_and_cursor() {
        let mut ed = editor();
        ed.set_padding_x(2);
        type_str(&mut ed, "x");
        let surf = ed.draw(&ctx(20, 10));
        // Content starts at the padding column.
        assert_eq!(surf.read_cell(2, 1).char.grapheme(), "x");
        let cursor = surf.cursor.expect("cursor reported");
        assert_eq!(cursor.col, 3);
    }

    // -- bindings table --

    #[test]
    fn bindings_cover_the_core_chords() {
        let bindings = TextArea::bindings();
        assert!(!bindings.is_empty());
        assert!(bindings.iter().any(|b| b.description.contains("Submit")));
        assert!(bindings.iter().any(|b| b.description.contains("Undo")));
        assert!(bindings.iter().any(|b| b.keys.contains("Ctrl-Y")));
        assert!(bindings.iter().any(|b| b.keys == "Ctrl-]"));
        assert!(bindings.iter().any(|b| b.keys == "Ctrl-Alt-]"));
    }

    // -- Paste markers: creation --

    #[test]
    fn creates_a_paste_marker_for_large_pastes() {
        let mut ed = editor();
        let text = paste_large(&mut ed);
        let (_len, _marker) = find_marker(&text);
    }

    // -- Paste markers: atomic navigation --

    #[test]
    fn treats_paste_marker_as_single_unit_for_right_arrow() {
        let mut ed = editor();
        type_str(&mut ed, "A");
        paste_large(&mut ed);
        type_str(&mut ed, "B");

        let text = ed.text();
        let (marker_len, _marker) = find_marker(&text);

        send(&mut ed, &ctrl('a'));
        assert_eq!(ed.cursor(), (0, 0));
        send(&mut ed, &right());
        assert_eq!(ed.cursor(), (0, 1));
        send(&mut ed, &right());
        assert_eq!(ed.cursor(), (0, 1 + marker_len));
        send(&mut ed, &right());
        assert_eq!(ed.cursor(), (0, 1 + marker_len + 1));
    }

    #[test]
    fn treats_paste_marker_as_single_unit_for_left_arrow() {
        let mut ed = editor();
        type_str(&mut ed, "A");
        paste_large(&mut ed);
        type_str(&mut ed, "B");

        let text = ed.text();
        let (marker_len, _marker) = find_marker(&text);

        send(&mut ed, &left());
        assert_eq!(ed.cursor(), (0, 1 + marker_len));
        send(&mut ed, &left());
        assert_eq!(ed.cursor(), (0, 1));
        send(&mut ed, &left());
        assert_eq!(ed.cursor(), (0, 0));
    }

    #[test]
    fn treats_paste_marker_as_single_unit_for_backspace() {
        let mut ed = editor();
        type_str(&mut ed, "A");
        paste_large(&mut ed);
        type_str(&mut ed, "B");

        let text = ed.text();
        let (marker_len, _marker) = find_marker(&text);

        send(&mut ed, &ctrl('a'));
        send(&mut ed, &right()); // past 'A'
        send(&mut ed, &right()); // past marker
        assert_eq!(ed.cursor(), (0, 1 + marker_len));

        send(&mut ed, &backspace());
        assert_eq!(ed.text(), "AB");
        assert_eq!(ed.cursor(), (0, 1));
    }

    #[test]
    fn treats_paste_marker_as_single_unit_for_forward_delete() {
        let mut ed = editor();
        type_str(&mut ed, "A");
        paste_large(&mut ed);
        type_str(&mut ed, "B");

        send(&mut ed, &ctrl('a'));
        send(&mut ed, &right()); // after 'A', on the marker
        send(&mut ed, &delete());
        assert_eq!(ed.text(), "AB");
        assert_eq!(ed.cursor(), (0, 1));
    }

    #[test]
    fn treats_paste_marker_as_single_unit_for_word_movement() {
        let mut ed = editor();
        type_str(&mut ed, "X ");
        paste_large(&mut ed);
        type_str(&mut ed, " Y");

        let text = ed.text();
        let (marker_len, _marker) = find_marker(&text);

        send(&mut ed, &ctrl('a'));
        // Word-right skips "X".
        send(&mut ed, &ctrl_right());
        assert_eq!(ed.cursor(), (0, 1));
        // Word-right skips the space and the whole marker atomically.
        send(&mut ed, &ctrl_right());
        assert_eq!(ed.cursor(), (0, 2 + marker_len));
    }

    #[test]
    fn undo_restores_marker_after_backspace_deletion() {
        let mut ed = editor();
        type_str(&mut ed, "A");
        paste_large(&mut ed);
        type_str(&mut ed, "B");

        let text_before = ed.text();

        send(&mut ed, &ctrl('a'));
        send(&mut ed, &right()); // past A
        send(&mut ed, &right()); // past marker
        send(&mut ed, &backspace());
        assert_eq!(ed.text(), "AB");

        send(&mut ed, &ctrl('-'));
        assert_eq!(ed.text(), text_before);
    }

    #[test]
    fn handles_multiple_paste_markers_in_same_line() {
        let mut ed = editor();
        paste_large(&mut ed);
        type_str(&mut ed, " ");
        paste_large(&mut ed);

        let text = ed.text();
        let (m0, _) = find_marker(&text);
        // The second marker begins after the first marker plus the space.
        let second = &text[m0 + 1..];
        let (m1, _) = find_marker(second);

        send(&mut ed, &ctrl('a'));
        send(&mut ed, &right()); // skip first marker
        assert_eq!(ed.cursor(), (0, m0));
        send(&mut ed, &right()); // past space
        assert_eq!(ed.cursor(), (0, m0 + 1));
        send(&mut ed, &right()); // skip second marker
        assert_eq!(ed.cursor(), (0, m0 + 1 + m1));
    }

    #[test]
    fn does_not_treat_manually_typed_marker_like_text_as_atomic() {
        let mut ed = editor();
        // Typing the marker-like string creates no paste entry, so the
        // validation rule keeps it as ordinary characters.
        let fake = "[paste #99 +5 lines]";
        type_str(&mut ed, fake);
        assert_eq!(ed.text(), fake);

        send(&mut ed, &ctrl('a'));
        send(&mut ed, &right());
        assert_eq!(ed.cursor(), (0, 1));
    }

    // -- Paste markers: expansion and submission --

    #[test]
    fn expands_large_pasted_content_literally_in_expanded_text() {
        let mut ed = editor();
        let pasted_text = [
            "line 1",
            "line 2",
            "line 3",
            "line 4",
            "line 5",
            "line 6",
            "line 7",
            "line 8",
            "line 9",
            "line 10",
            "tokens $1 $2 $& $$ $` $' end",
        ]
        .join("\n");

        send(&mut ed, &paste(&pasted_text));

        let text = ed.text();
        let (_len, _marker) = find_marker(&text);
        assert_eq!(ed.expanded_text(), pasted_text);
    }

    #[test]
    fn submits_large_pasted_content_literally() {
        let submitted: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let captured = Rc::clone(&submitted);

        let mut ed = editor();
        ed.on_submit = Some(Box::new(move |_ctx, text| {
            *captured.borrow_mut() = text.to_string();
        }));

        let pasted_text = [
            "line 1",
            "line 2",
            "line 3",
            "line 4",
            "line 5",
            "line 6",
            "line 7",
            "line 8",
            "line 9",
            "line 10",
            "tokens $1 $2 $& $$ $` $' end",
        ]
        .join("\n");

        send(&mut ed, &paste(&pasted_text));
        send(&mut ed, &key(Key::ENTER, Modifiers::empty()));

        assert_eq!(submitted.borrow().as_str(), pasted_text);
    }

    // -- Paste markers: small-paste tab handling --

    #[test]
    fn small_paste_expands_each_tab_to_four_spaces() {
        let mut ed = editor();
        send(&mut ed, &paste("a\tb\tc"));
        assert_eq!(ed.text(), "a    b    c");
    }

    #[test]
    fn small_paste_with_tabs_and_newlines_preserves_both() {
        let mut ed = editor();
        send(&mut ed, &paste("\tone\n\ttwo"));
        assert_eq!(ed.text(), "    one\n    two");
    }

    #[test]
    fn small_paste_strips_non_tab_control_chars() {
        let mut ed = editor();
        send(&mut ed, &paste("a\tb\0\x07c"));
        assert_eq!(ed.text(), "a    bc");
    }

    #[test]
    fn decodes_csi_u_ctrl_letters_inside_bracketed_paste() {
        // Some terminals re-encode a paste's embedded newlines as the Kitty
        // CSI-u Ctrl+J sequence `ESC [ 106 ; 5 u`. The paste pipeline must
        // decode those to `\n` before the per-char control filter runs, or the
        // filter strips the ESC and leaks the printable tail (`[106;5u`).
        let mut ed = editor();
        send(&mut ed, &paste("line1\x1b[106;5uline2\x1b[106;5uline3"));
        assert_eq!(ed.text(), "line1\nline2\nline3");
    }

    // -- Paste markers: path-prefix safety --

    #[test]
    fn paste_with_slash_prefix_after_word_prepends_space() {
        let mut ed = editor();
        type_str(&mut ed, "cd");
        send(&mut ed, &paste("/etc/hosts"));
        assert_eq!(ed.text(), "cd /etc/hosts");
    }

    #[test]
    fn paste_with_tilde_prefix_after_word_prepends_space() {
        let mut ed = editor();
        type_str(&mut ed, "ls");
        send(&mut ed, &paste("~/code"));
        assert_eq!(ed.text(), "ls ~/code");
    }

    #[test]
    fn paste_with_dot_prefix_after_word_prepends_space() {
        let mut ed = editor();
        type_str(&mut ed, "ls");
        send(&mut ed, &paste("./bin"));
        assert_eq!(ed.text(), "ls ./bin");
    }

    #[test]
    fn paste_with_path_prefix_after_space_does_not_prepend() {
        let mut ed = editor();
        type_str(&mut ed, "cd ");
        send(&mut ed, &paste("/etc/hosts"));
        assert_eq!(ed.text(), "cd /etc/hosts");
    }

    #[test]
    fn paste_with_path_prefix_at_start_of_line_does_not_prepend() {
        let mut ed = editor();
        send(&mut ed, &paste("/etc/hosts"));
        assert_eq!(ed.text(), "/etc/hosts");
    }

    #[test]
    fn paste_with_path_prefix_after_punctuation_does_not_prepend() {
        let mut ed = editor();
        type_str(&mut ed, "(");
        send(&mut ed, &paste("/etc"));
        assert_eq!(ed.text(), "(/etc");
    }

    #[test]
    fn paste_without_path_prefix_after_word_does_not_prepend() {
        let mut ed = editor();
        type_str(&mut ed, "foo");
        send(&mut ed, &paste("bar"));
        assert_eq!(ed.text(), "foobar");
    }

    #[test]
    fn large_paste_with_path_prefix_after_word_separates_marker_with_space() {
        let mut ed = editor();
        type_str(&mut ed, "cd");
        let big = "/path\n".repeat(20).trim_end().to_string();
        send(&mut ed, &paste(&big));

        let text = ed.text();
        assert!(
            text.starts_with("cd ["),
            "expected `cd ` + marker, got {text:?}"
        );
        assert_eq!(ed.expanded_text(), format!("cd {big}"));
    }

    // -- Paste markers: layout interaction --

    #[test]
    fn does_not_crash_when_paste_marker_is_wider_than_terminal_width() {
        let mut ed = editor();
        paste_n_lines(&mut ed, 47);

        let text = ed.text();
        let (marker_len, _marker) = find_marker(&text);

        // Draw narrow. The +1 offsets the reserved caret column so the layout
        // width is 7. The marker (20 chars) is wider than that and must split.
        let _ = ed.draw(&ctx(8, 40));
        assert!(
            marker_len > ed.layout_width,
            "marker ({marker_len} chars) should exceed layout width {}",
            ed.layout_width,
        );
        let vls = ed.compute_visual_line_map(ed.layout_width);
        assert!(
            vls.len() > 1,
            "wide marker should split across visual lines"
        );
        assert_all_vls_fit(&ed);
    }

    #[test]
    fn does_not_crash_when_text_plus_marker_exceeds_width_with_cursor_on_marker() {
        let mut ed = editor();
        type_str(&mut ed, &"b".repeat(35));
        paste_n_lines(&mut ed, 27);
        type_str(&mut ed, &"b".repeat(4));

        // Move the cursor left so it lands atomically on the marker.
        for _ in 0..5 {
            send(&mut ed, &left());
        }
        let _ = ed.draw(&ctx(55, 40));
        assert_all_vls_fit(&ed);
    }

    #[test]
    fn wide_marker_split_reconstructs_line_byte_for_byte() {
        // A marker wider than the layout width sub-splits at grapheme
        // granularity. The split is purely visual, so concatenating the spans
        // must still yield the logical line exactly, with the marker bytes
        // intact and the spans contiguous. A drifted sub-span offset would
        // desync the visual-line map from the cursor math.
        let mut ed = editor();
        type_str(&mut ed, "abc");
        paste_n_lines(&mut ed, 47);
        type_str(&mut ed, "xyz");
        let _ = ed.draw(&ctx(8, 40)); // layout width 7, narrower than the marker

        let line = ed.lines[0].clone();
        let spans = ed.wrap_line_spans(&line, ed.layout_width);
        assert!(spans.len() > 1, "wide marker should split across rows");

        let joined: String = spans.iter().map(|&(s, e)| &line[s..e]).collect();
        assert_eq!(joined, line, "spans must reconstruct the marker line");

        let mut prev_end = 0;
        for &(s, e) in &spans {
            assert_eq!(s, prev_end, "spans must be contiguous");
            assert!(s <= e && e <= line.len());
            prev_end = e;
        }
        assert_eq!(prev_end, line.len(), "spans must cover the line");

        assert_all_vls_fit(&ed);
    }

    #[test]
    fn wrap_re_checks_overflow_after_backtracking() {
        // After backtracking to the space, the run of 35 b's plus the atomic
        // marker must re-check overflow and force-break rather than overflow.
        let mut ed = editor();
        type_str(&mut ed, " ");
        type_str(&mut ed, &"b".repeat(35));
        paste_n_lines(&mut ed, 27);
        type_str(&mut ed, &"b".repeat(4));

        let _ = ed.draw(&ctx(55, 40));
        assert_all_vls_fit(&ed);
    }

    #[test]
    fn snaps_to_paste_marker_start_when_navigating_down_into_it() {
        let mut ed = editor();
        ed.set_text("12345678901234567890\n\nhello ");
        paste_n_chars(&mut ed, 2000);
        let _ = ed.draw(&ctx(80, 40));

        let text = ed.text();
        let (_len, marker) = find_marker(&text);
        assert!(
            marker.contains("chars]"),
            "expected chars marker in {text:?}"
        );

        send(&mut ed, &up());
        send(&mut ed, &up());
        send(&mut ed, &ctrl('a'));
        for _ in 0..10 {
            send(&mut ed, &right());
        }
        assert_eq!(ed.cursor(), (0, 10));

        send(&mut ed, &down());
        assert_eq!(ed.cursor(), (1, 0));

        // Sticky col 10 falls inside the marker (which starts at col 6), so the
        // cursor snaps to the marker start rather than into it.
        send(&mut ed, &down());
        assert_eq!(ed.cursor(), (2, 6));
    }

    #[test]
    fn preserves_sticky_column_when_navigating_through_paste_marker_line() {
        let mut ed = editor();
        type_str(&mut ed, "1234567890123456");
        send(&mut ed, &shift_enter());
        send(&mut ed, &shift_enter());
        paste_n_chars(&mut ed, 2000);
        send(&mut ed, &shift_enter());
        send(&mut ed, &shift_enter());
        type_str(&mut ed, "abcdefghijklmnop");
        let _ = ed.draw(&ctx(30, 40));

        for _ in 0..4 {
            send(&mut ed, &up());
        }
        send(&mut ed, &ctrl('a'));
        for _ in 0..10 {
            send(&mut ed, &right());
        }
        assert_eq!(ed.cursor(), (0, 10));

        send(&mut ed, &down());
        assert_eq!(ed.cursor(), (1, 0));
        // Snap onto the marker start (col 0).
        send(&mut ed, &down());
        assert_eq!(ed.cursor(), (2, 0));
        send(&mut ed, &down());
        assert_eq!(ed.cursor(), (3, 0));
        // Sticky col 10 restores on the last line.
        send(&mut ed, &down());
        assert_eq!(ed.cursor(), (4, 10));
    }

    #[test]
    fn does_not_get_stuck_moving_down_from_a_multi_visual_line_paste_marker() {
        let mut ed = editor();
        type_str(&mut ed, "abcdefgh");
        paste_n_lines(&mut ed, 100);
        type_str(&mut ed, "ijklmnopqr");
        send(&mut ed, &shift_enter());
        type_str(&mut ed, "123456789012345678");
        // ctx width 21 leaves layout width 20 after the reserved caret column,
        // matching the geometry the assertions assume.
        let _ = ed.draw(&ctx(21, 40));

        let text = ed.text();
        let (marker_len, _marker) = find_marker(&text);
        assert!(
            marker_len > 20,
            "marker ({marker_len} chars) should exceed layout width 20",
        );
        let marker_start = 8;
        let marker_end = marker_start + marker_len;

        send(&mut ed, &up());
        send(&mut ed, &ctrl('a'));
        for _ in 0..6 {
            send(&mut ed, &right());
        }
        assert_eq!(ed.cursor(), (0, 6));

        // Down lands on the marker start.
        send(&mut ed, &down());
        assert_eq!(ed.cursor(), (0, marker_start));
        // Down again: preferred col 6 lands past the marker tail, on the first
        // content char after the marker, without snapping back in.
        send(&mut ed, &down());
        assert_eq!(ed.cursor(), (0, marker_end));
        // Round-trip back up.
        send(&mut ed, &up());
        assert_eq!(ed.cursor(), (0, marker_start));
        send(&mut ed, &up());
        assert_eq!(ed.cursor(), (0, 6));
    }

    #[test]
    fn skips_marker_continuation_vls_when_preferred_col_falls_in_marker_tail() {
        let mut ed = editor();
        type_str(&mut ed, "abcdefgh");
        paste_n_lines(&mut ed, 100);
        type_str(&mut ed, "ijklmnopqr");
        send(&mut ed, &shift_enter());
        type_str(&mut ed, "123456789012345678");
        let _ = ed.draw(&ctx(21, 40));

        send(&mut ed, &up());
        send(&mut ed, &ctrl('a'));
        for _ in 0..3 {
            send(&mut ed, &right());
        }
        assert_eq!(ed.cursor(), (0, 3));

        // Down: marker start.
        send(&mut ed, &down());
        assert_eq!(ed.cursor().1, 8);
        // Down: preferred col 3 falls in the marker-tail continuation VL, so the
        // move skips forward to line 1.
        send(&mut ed, &down());
        assert_eq!(ed.cursor(), (1, 3));
        // Round-trip back.
        send(&mut ed, &up());
        assert_eq!(ed.cursor().1, 8);
        send(&mut ed, &up());
        assert_eq!(ed.cursor(), (0, 3));
    }

    // -- Char-jump: forward --

    #[test]
    fn jumps_forward_to_first_occurrence_on_same_line() {
        let mut ed = editor();
        ed.set_text("hello world");
        send(&mut ed, &ctrl('a'));
        send(&mut ed, &jump_forward());
        send(&mut ed, &char_key('o'));
        assert_eq!(ed.cursor(), (0, 4));
    }

    #[test]
    fn jumps_forward_to_next_occurrence_after_cursor() {
        let mut ed = editor();
        ed.set_text("hello world");
        send(&mut ed, &ctrl('a'));
        for _ in 0..4 {
            send(&mut ed, &right());
        }
        assert_eq!(ed.cursor(), (0, 4));
        send(&mut ed, &jump_forward());
        send(&mut ed, &char_key('o'));
        assert_eq!(ed.cursor(), (0, 7));
    }

    #[test]
    fn jumps_forward_across_multiple_lines() {
        let mut ed = editor();
        ed.set_text("abc\ndef\nghi");
        send(&mut ed, &up());
        send(&mut ed, &up());
        send(&mut ed, &ctrl('a'));
        assert_eq!(ed.cursor(), (0, 0));
        send(&mut ed, &jump_forward());
        send(&mut ed, &char_key('g'));
        assert_eq!(ed.cursor(), (2, 0));
    }

    // -- Char-jump: backward --

    #[test]
    fn jumps_backward_to_occurrence_before_cursor_on_same_line() {
        let mut ed = editor();
        ed.set_text("hello world");
        assert_eq!(ed.cursor(), (0, 11));
        send(&mut ed, &jump_backward());
        send(&mut ed, &char_key('o'));
        assert_eq!(ed.cursor(), (0, 7));
    }

    #[test]
    fn jumps_backward_across_multiple_lines() {
        let mut ed = editor();
        ed.set_text("abc\ndef\nghi");
        assert_eq!(ed.cursor(), (2, 3));
        send(&mut ed, &jump_backward());
        send(&mut ed, &char_key('a'));
        assert_eq!(ed.cursor(), (0, 0));
    }

    // -- Char-jump: no-match, case sensitivity --

    #[test]
    fn jump_does_nothing_when_character_not_found_forward() {
        let mut ed = editor();
        ed.set_text("hello world");
        send(&mut ed, &ctrl('a'));
        send(&mut ed, &jump_forward());
        send(&mut ed, &char_key('z'));
        assert_eq!(ed.cursor(), (0, 0));
    }

    #[test]
    fn jump_does_nothing_when_character_not_found_backward() {
        let mut ed = editor();
        ed.set_text("hello world");
        send(&mut ed, &jump_backward());
        send(&mut ed, &char_key('z'));
        assert_eq!(ed.cursor(), (0, 11));
    }

    #[test]
    fn jump_is_case_sensitive() {
        let mut ed = editor();
        ed.set_text("Hello World");
        send(&mut ed, &ctrl('a'));
        // No lowercase 'h' exists, so the jump is a no-op.
        send(&mut ed, &jump_forward());
        send(&mut ed, &char_key('h'));
        assert_eq!(ed.cursor(), (0, 0));
        // Uppercase 'W' does exist.
        send(&mut ed, &jump_forward());
        send(&mut ed, &char_key('W'));
        assert_eq!(ed.cursor(), (0, 6));
    }

    // -- Char-jump: cancelling --

    #[test]
    fn cancels_jump_mode_when_ctrl_bracket_is_pressed_again() {
        let mut ed = editor();
        ed.set_text("hello world");
        send(&mut ed, &ctrl('a'));
        send(&mut ed, &jump_forward()); // enter
        send(&mut ed, &jump_forward()); // cancel
        // 'o' is now a normal insert, not a jump target.
        send(&mut ed, &char_key('o'));
        assert_eq!(ed.text(), "ohello world");
    }

    #[test]
    fn cancels_backward_jump_mode_when_ctrl_alt_bracket_is_pressed_again() {
        let mut ed = editor();
        ed.set_text("hello world");
        send(&mut ed, &jump_backward()); // enter
        send(&mut ed, &jump_backward()); // cancel
        send(&mut ed, &char_key('o'));
        assert_eq!(ed.text(), "hello worldo");
    }

    #[test]
    fn cancels_jump_mode_on_escape_and_does_not_consume_the_escape() {
        let mut ed = editor();
        ed.set_text("hello world");
        send(&mut ed, &ctrl('a'));
        send(&mut ed, &jump_forward());
        // Escape must fall through so the parent surface can handle it.
        let consumed = send_consumed(&mut ed, &escape());
        assert!(!consumed, "escape in jump mode must not be consumed");
        assert_eq!(ed.cursor(), (0, 0));
        // The silent fall-through cleared the mode, so 'o' inserts as text.
        send(&mut ed, &char_key('o'));
        assert_eq!(ed.text(), "ohello world");
    }

    // -- Char-jump: special chars, empty text, last_action reset --

    #[test]
    fn jump_searches_for_special_characters() {
        let mut ed = editor();
        ed.set_text("foo(bar) = baz;");
        send(&mut ed, &ctrl('a'));
        send(&mut ed, &jump_forward());
        send(&mut ed, &char_key('('));
        assert_eq!(ed.cursor(), (0, 3));
        send(&mut ed, &jump_forward());
        send(&mut ed, &char_key('='));
        assert_eq!(ed.cursor(), (0, 9));
    }

    #[test]
    fn jump_handles_empty_text_gracefully() {
        let mut ed = editor();
        ed.set_text("");
        send(&mut ed, &jump_forward());
        send(&mut ed, &char_key('x'));
        assert_eq!(ed.cursor(), (0, 0));
    }

    #[test]
    fn jumping_resets_last_action_so_following_type_starts_new_undo_unit() {
        let mut ed = editor();
        ed.set_text("hello world");
        send(&mut ed, &ctrl('a'));

        // Typing sets last_action = TypeWord.
        send(&mut ed, &char_key('x'));
        assert_eq!(ed.text(), "xhello world");

        send(&mut ed, &jump_forward());
        send(&mut ed, &char_key('o'));

        // The jump reset last_action, so this type starts a fresh undo unit.
        send(&mut ed, &char_key('Y'));
        assert_eq!(ed.text(), "xhellYo world");

        send(&mut ed, &ctrl('-'));
        assert_eq!(ed.text(), "xhello world");
    }

    // -- Autocomplete --

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use tokio::sync::Notify;

    use crate::vxfw::CompletionApplied;

    /// A `Tab` key press.
    fn tab() -> Event {
        key(Key::TAB, Modifiers::empty())
    }

    /// Convenience: item whose value and label are the same, no description.
    fn item(v: &str) -> AutocompleteItem {
        AutocompleteItem::new(v.to_string(), v.to_string())
    }

    /// Standard apply behavior: replace exactly `prefix.len()` bytes before the
    /// cursor with the item's value, moving the cursor to the end of the insert.
    fn apply_prefix_replace(
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionApplied {
        let mut new_lines = lines.to_vec();
        let line = new_lines[cursor_line].clone();
        let before = &line[..cursor_col - prefix.len()];
        let after = &line[cursor_col..];
        new_lines[cursor_line] = format!("{}{}{}", before, item.value, after);
        CompletionApplied {
            lines: new_lines,
            cursor_line,
            cursor_col: cursor_col - prefix.len() + item.value.len(),
        }
    }

    /// A closure-backed provider that returns `(items, prefix)` given `(lines,
    /// cursor_line, cursor_col, force)`. Lets a test control provider behavior
    /// inline without a full named type.
    struct MockProvider<F>
    where
        F: Fn(&[String], usize, usize, bool) -> Option<(Vec<AutocompleteItem>, String)>,
    {
        get: F,
    }

    #[async_trait]
    impl<F> AutocompleteProvider for MockProvider<F>
    where
        F: Fn(&[String], usize, usize, bool) -> Option<(Vec<AutocompleteItem>, String)>
            + Send
            + Sync
            + 'static,
    {
        async fn get_suggestions(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            opts: SuggestOpts,
        ) -> Option<AutocompleteSuggestions> {
            let (items, prefix) = (self.get)(lines, cursor_line, cursor_col, opts.force)?;
            Some(AutocompleteSuggestions { items, prefix })
        }

        fn apply_completion(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> CompletionApplied {
            apply_prefix_replace(lines, cursor_line, cursor_col, item, prefix)
        }
    }

    /// Drives autocomplete to a settled state for a deterministic test: awaits
    /// the pending worker (so its delivery is on the channel), then pumps. The
    /// widget still owns the receiver in tests, so `pump_autocomplete` drains it
    /// in place, exactly as the host's `select!` arm would call
    /// `apply_autocomplete_delivery`.
    async fn wait_autocomplete(ed: &mut TextArea) {
        if let Some(handle) = ed.autocomplete_task.take() {
            let _ = handle.await;
        }
        ed.pump_autocomplete();
    }

    /// Types `s` one key at a time, settling autocomplete after each keystroke.
    async fn type_settle(ed: &mut TextArea, s: &str) {
        for c in s.chars() {
            send(ed, &char_key(c));
            wait_autocomplete(ed).await;
        }
    }

    #[tokio::test]
    async fn auto_applies_single_force_file_suggestion_without_showing_menu() {
        let mut ed = editor();
        ed.set_autocomplete_provider(Arc::new(MockProvider {
            get: |lines, _l, col, force| {
                if !force {
                    return None;
                }
                let prefix = &lines[0][..col];
                if prefix == "Work" {
                    Some((vec![item("Workspace/")], "Work".to_string()))
                } else {
                    None
                }
            },
        }));

        type_settle(&mut ed, "Work").await;
        assert_eq!(ed.text(), "Work");

        // Tab auto-applies the single suggestion.
        send(&mut ed, &tab());
        wait_autocomplete(&mut ed).await;
        assert_eq!(ed.text(), "Workspace/");
        assert!(!ed.is_showing_autocomplete());

        // Undo restores "Work".
        send(&mut ed, &ctrl('-'));
        assert_eq!(ed.text(), "Work");
    }

    #[tokio::test]
    async fn shows_menu_when_force_file_has_multiple_suggestions() {
        let mut ed = editor();
        ed.set_autocomplete_provider(Arc::new(MockProvider {
            get: |lines, _l, col, force| {
                if !force {
                    return None;
                }
                let prefix = &lines[0][..col];
                if prefix == "src" {
                    Some((vec![item("src/"), item("src.txt")], "src".to_string()))
                } else {
                    None
                }
            },
        }));

        type_settle(&mut ed, "src").await;

        // Tab shows the menu (multiple suggestions).
        send(&mut ed, &tab());
        wait_autocomplete(&mut ed).await;
        assert_eq!(ed.text(), "src");
        assert!(ed.is_showing_autocomplete());

        // A second Tab applies the highlighted (first) suggestion.
        send(&mut ed, &tab());
        wait_autocomplete(&mut ed).await;
        assert_eq!(ed.text(), "src/");
        assert!(!ed.is_showing_autocomplete());
    }

    #[tokio::test]
    async fn keeps_suggestions_open_when_typing_in_force_mode() {
        let all_files = [
            item("readme.md"),
            item("package.json"),
            item("src/"),
            item("dist/"),
        ];
        let mut ed = editor();
        ed.set_autocomplete_provider(Arc::new(MockProvider {
            get: move |lines, _l, col, force| {
                let prefix = &lines[0][..col];
                let should_match = force || prefix.contains('/') || prefix.starts_with('.');
                if !should_match {
                    return None;
                }
                let filtered: Vec<AutocompleteItem> = all_files
                    .iter()
                    .filter(|f| f.value.to_lowercase().starts_with(&prefix.to_lowercase()))
                    .cloned()
                    .collect();
                if filtered.is_empty() {
                    return None;
                }
                Some((filtered, prefix.to_string()))
            },
        }));

        // Tab on an empty prompt: force mode, shows all.
        send(&mut ed, &tab());
        wait_autocomplete(&mut ed).await;
        assert!(ed.is_showing_autocomplete());

        // Typing narrows but stays in force mode.
        send(&mut ed, &char_key('r'));
        wait_autocomplete(&mut ed).await;
        assert_eq!(ed.text(), "r");
        assert!(ed.is_showing_autocomplete());

        send(&mut ed, &char_key('e'));
        wait_autocomplete(&mut ed).await;
        assert_eq!(ed.text(), "re");
        assert!(ed.is_showing_autocomplete());

        // Tab applies the first remaining suggestion ("readme.md").
        send(&mut ed, &tab());
        wait_autocomplete(&mut ed).await;
        assert_eq!(ed.text(), "readme.md");
        assert!(!ed.is_showing_autocomplete());
    }

    /// The `@`-attachment context debounces: a synchronous burst of keystrokes
    /// coalesces into a single provider call once the window expires.
    #[tokio::test]
    async fn debounces_at_autocomplete_while_typing() {
        struct RecordingProvider {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl AutocompleteProvider for RecordingProvider {
            async fn get_suggestions(
                &self,
                lines: &[String],
                _cursor_line: usize,
                cursor_col: usize,
                _opts: SuggestOpts,
            ) -> Option<AutocompleteSuggestions> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let before = &lines[0][..cursor_col];
                if before.contains('@') {
                    Some(AutocompleteSuggestions {
                        prefix: before.rsplit(' ').next().unwrap_or("").to_string(),
                        items: vec![AutocompleteItem::new("@file.rs", "file.rs")],
                    })
                } else {
                    None
                }
            }
            fn apply_completion(
                &self,
                lines: &[String],
                cursor_line: usize,
                cursor_col: usize,
                item: &AutocompleteItem,
                _prefix: &str,
            ) -> CompletionApplied {
                CompletionApplied {
                    lines: lines.to_vec(),
                    cursor_line,
                    cursor_col: cursor_col + item.value.len(),
                }
            }
        }

        let mut ed = editor();
        let calls = Arc::new(AtomicUsize::new(0));
        ed.set_autocomplete_provider(Arc::new(RecordingProvider {
            calls: Arc::clone(&calls),
        }));

        // A rapid burst inside an `@` context: no keystroke is awaited, so no
        // worker runs and the debounce window has not expired.
        for ch in "@abcdefgh".chars() {
            send(&mut ed, &char_key(ch));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "provider must not be called until the @ debounce window expires",
        );

        wait_autocomplete(&mut ed).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "exactly one debounced provider call should land after the @ burst",
        );
        assert!(ed.is_showing_autocomplete());
    }

    /// A `#` at a token boundary is a symbol trigger just like `@`, and debounces
    /// the same way.
    #[tokio::test]
    async fn debounces_hash_autocomplete_while_typing() {
        struct RecordingProvider {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl AutocompleteProvider for RecordingProvider {
            async fn get_suggestions(
                &self,
                lines: &[String],
                _cursor_line: usize,
                cursor_col: usize,
                _opts: SuggestOpts,
            ) -> Option<AutocompleteSuggestions> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let before = &lines[0][..cursor_col];
                Some(AutocompleteSuggestions {
                    prefix: before.to_string(),
                    items: vec![AutocompleteItem::new("#2983", "#2983")],
                })
            }
            fn apply_completion(
                &self,
                lines: &[String],
                cursor_line: usize,
                cursor_col: usize,
                item: &AutocompleteItem,
                _prefix: &str,
            ) -> CompletionApplied {
                CompletionApplied {
                    lines: lines.to_vec(),
                    cursor_line,
                    cursor_col: cursor_col + item.value.len(),
                }
            }
        }

        let mut ed = editor();
        let calls = Arc::new(AtomicUsize::new(0));
        ed.set_autocomplete_provider(Arc::new(RecordingProvider {
            calls: Arc::clone(&calls),
        }));

        for ch in "#2983".chars() {
            send(&mut ed, &char_key(ch));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "provider must not be called until the # debounce window expires",
        );

        wait_autocomplete(&mut ed).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "exactly one debounced provider call should land after the # burst",
        );
        assert!(ed.is_showing_autocomplete());
    }

    /// A new request cancels the in-flight one: the first, slow call observes
    /// cancellation when a second request supersedes it.
    #[tokio::test]
    async fn aborts_active_at_autocomplete_when_typing_continues() {
        struct SlowProvider {
            calls: Arc<AtomicUsize>,
            release: Arc<Notify>,
            first_call_saw_cancel: Arc<AtomicBool>,
        }
        #[async_trait]
        impl AutocompleteProvider for SlowProvider {
            async fn get_suggestions(
                &self,
                _lines: &[String],
                _cursor_line: usize,
                _cursor_col: usize,
                opts: SuggestOpts,
            ) -> Option<AutocompleteSuggestions> {
                let call_n = self.calls.fetch_add(1, Ordering::SeqCst);
                if call_n == 0 {
                    // First call: wait to be released, or to be cancelled.
                    tokio::select! {
                        _ = opts.cancel.cancelled() => {
                            self.first_call_saw_cancel.store(true, Ordering::SeqCst);
                            return None;
                        }
                        _ = self.release.notified() => {}
                    }
                    return None;
                }
                Some(AutocompleteSuggestions {
                    prefix: "@".to_string(),
                    items: vec![AutocompleteItem::new("@file.rs", "file.rs")],
                })
            }
            fn apply_completion(
                &self,
                lines: &[String],
                cursor_line: usize,
                cursor_col: usize,
                _item: &AutocompleteItem,
                _prefix: &str,
            ) -> CompletionApplied {
                CompletionApplied {
                    lines: lines.to_vec(),
                    cursor_line,
                    cursor_col,
                }
            }
        }

        let mut ed = editor();
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let first_call_saw_cancel = Arc::new(AtomicBool::new(false));
        ed.set_autocomplete_provider(Arc::new(SlowProvider {
            calls: Arc::clone(&calls),
            release: Arc::clone(&release),
            first_call_saw_cancel: Arc::clone(&first_call_saw_cancel),
        }));

        // First request. Tab fires immediately (no @ debounce).
        send(&mut ed, &tab());
        // Give the worker time to reach the `select!` inside get_suggestions.
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Second Tab: cancels the first request before releasing it.
        send(&mut ed, &tab());
        wait_autocomplete(&mut ed).await;

        // Nobody should be blocked anymore, but make sure.
        release.notify_waiters();
        tokio::task::yield_now().await;

        assert!(
            first_call_saw_cancel.load(Ordering::SeqCst),
            "the first in-flight request must observe cancellation when superseded",
        );
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "both requests must dispatch",
        );
    }

    /// A counter-backed provider that records how often it is asked and always
    /// returns nothing.
    struct CountingProvider {
        count: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl AutocompleteProvider for CountingProvider {
        async fn get_suggestions(
            &self,
            _lines: &[String],
            _cursor_line: usize,
            _cursor_col: usize,
            _opts: SuggestOpts,
        ) -> Option<AutocompleteSuggestions> {
            self.count.fetch_add(1, Ordering::SeqCst);
            None
        }
        fn apply_completion(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            item: &AutocompleteItem,
            _prefix: &str,
        ) -> CompletionApplied {
            CompletionApplied {
                lines: lines.to_vec(),
                cursor_line,
                cursor_col: cursor_col + item.value.len(),
            }
        }
    }

    fn counting_editor() -> (TextArea, Arc<AtomicUsize>) {
        let mut ed = editor();
        let count = Arc::new(AtomicUsize::new(0));
        ed.set_autocomplete_provider(Arc::new(CountingProvider {
            count: Arc::clone(&count),
        }));
        (ed, count)
    }

    #[tokio::test]
    async fn typing_prose_does_not_call_provider() {
        let (mut ed, count) = counting_editor();
        type_settle(&mut ed, "hello world ").await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "prose with no @/# trigger must not query the provider",
        );
    }

    #[tokio::test]
    async fn typing_a_bare_space_does_not_call_provider() {
        let (mut ed, count) = counting_editor();
        send(&mut ed, &char_key(' '));
        wait_autocomplete(&mut ed).await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn at_sign_after_whitespace_calls_provider() {
        let (mut ed, count) = counting_editor();
        send(&mut ed, &char_key('@'));
        wait_autocomplete(&mut ed).await;
        let after_bare_at = count.load(Ordering::SeqCst);
        assert!(after_bare_at >= 1, "a bare `@` should query the provider");

        type_settle(&mut ed, "hi @").await;
        assert!(
            count.load(Ordering::SeqCst) > after_bare_at,
            "`@` after whitespace should query the provider again",
        );
    }

    #[tokio::test]
    async fn at_sign_inside_a_word_does_not_call_provider() {
        let (mut ed, count) = counting_editor();
        type_settle(&mut ed, "user").await;
        let before_at = count.load(Ordering::SeqCst);
        send(&mut ed, &char_key('@'));
        wait_autocomplete(&mut ed).await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            before_at,
            "`@` immediately after a word must not open the popup",
        );
    }

    /// Regression: with an `@` popup open, typing a space leaves the `@` token,
    /// so the Regular popup closes rather than re-dispatching into a raw
    /// directory listing of the working directory.
    #[tokio::test]
    async fn space_leaving_at_token_closes_regular_popup() {
        let mut ed = editor();
        ed.set_autocomplete_provider(Arc::new(MockProvider {
            // Matches on any text still containing an `@`, so a re-dispatch after
            // the space would keep the popup open if the gate did not fire.
            get: |lines, _l, col, _force| {
                let before = &lines[0][..col];
                let at_idx = before.rfind('@')?;
                let prefix = &before[at_idx..];
                Some((vec![item("@src/main.rs")], prefix.to_string()))
            },
        }));

        type_settle(&mut ed, "@ma").await;
        assert!(ed.is_showing_autocomplete(), "the @ popup should be open");

        send(&mut ed, &char_key(' '));
        wait_autocomplete(&mut ed).await;
        assert!(
            !ed.is_showing_autocomplete(),
            "leaving the @ token with a space must close the popup",
        );
        assert!(ed.autocomplete_state.is_none());
    }

    /// A narrowing character typed inside the `@` token keeps the Regular popup
    /// open, distinguishing genuine narrowing from leaving the token.
    #[tokio::test]
    async fn narrowing_inside_at_token_keeps_regular_popup_open() {
        let mut ed = editor();
        ed.set_autocomplete_provider(Arc::new(MockProvider {
            get: |lines, _l, col, _force| {
                let before = &lines[0][..col];
                let at_idx = before.rfind('@')?;
                let prefix = &before[at_idx..];
                Some((vec![item("@abc.rs")], prefix.to_string()))
            },
        }));

        type_settle(&mut ed, "@ab").await;
        assert!(ed.is_showing_autocomplete());

        send(&mut ed, &char_key('c'));
        wait_autocomplete(&mut ed).await;
        assert_eq!(ed.text(), "@abc");
        assert!(
            ed.is_showing_autocomplete(),
            "narrowing within the @ token must keep the popup open",
        );
    }

    #[tokio::test]
    async fn enter_on_at_file_popup_applies_completion_but_does_not_submit() {
        struct AtProvider;
        #[async_trait]
        impl AutocompleteProvider for AtProvider {
            async fn get_suggestions(
                &self,
                lines: &[String],
                _cursor_line: usize,
                cursor_col: usize,
                _opts: SuggestOpts,
            ) -> Option<AutocompleteSuggestions> {
                let before = &lines[0][..cursor_col];
                let at_idx = before.rfind('@')?;
                let prefix = &before[at_idx..];
                Some(AutocompleteSuggestions {
                    prefix: prefix.to_string(),
                    items: vec![AutocompleteItem {
                        value: format!("{}src/main.rs", &prefix[..1]),
                        label: "src/main.rs".to_string(),
                        description: None,
                    }],
                })
            }
            fn apply_completion(
                &self,
                lines: &[String],
                cursor_line: usize,
                cursor_col: usize,
                item: &AutocompleteItem,
                prefix: &str,
            ) -> CompletionApplied {
                let mut new_lines = lines.to_vec();
                let line = new_lines[cursor_line].clone();
                let before = &line[..cursor_col - prefix.len()];
                let after = &line[cursor_col..];
                new_lines[cursor_line] = format!("{}{}{}", before, item.value, after);
                CompletionApplied {
                    lines: new_lines,
                    cursor_line,
                    cursor_col: before.len() + item.value.len(),
                }
            }
        }

        let mut ed = editor();
        ed.set_autocomplete_provider(Arc::new(AtProvider));

        type_settle(&mut ed, "look at @").await;
        assert!(ed.is_showing_autocomplete());

        send(&mut ed, &key(Key::ENTER, Modifiers::empty()));
        assert_eq!(
            ed.take_submitted(),
            None,
            "Enter on an @-file popup applies the completion, it does not submit",
        );
        assert!(
            !ed.is_showing_autocomplete(),
            "the popup is dismissed after Enter",
        );
        assert_eq!(ed.text(), "look at @src/main.rs");
    }

    #[tokio::test]
    async fn at_prefix_autocomplete_draws_single_full_path_column() {
        let mut ed = editor();
        ed.set_autocomplete_provider(Arc::new(MockProvider {
            get: |lines, _l, col, _force| {
                let before = &lines[0][..col];
                if !before.contains('@') {
                    return None;
                }
                // Fuzzy file items carry the full relative path in
                // `description` and the bare filename in `label`.
                Some((
                    vec![
                        AutocompleteItem::new("@src/main.rs", "main.rs")
                            .with_description("src/main.rs"),
                        AutocompleteItem::new("@src/other.rs", "other.rs")
                            .with_description("src/other.rs"),
                    ],
                    "@".to_string(),
                ))
            },
        }));

        send(&mut ed, &char_key('@'));
        wait_autocomplete(&mut ed).await;
        assert!(ed.is_showing_autocomplete());

        // The popup is an overlay surface, not part of the editor's own draw.
        // Draw the editor first so the popup surface reads the width method and
        // theme the editor stashed, then render the overlay.
        let _ = ed.draw(&ctx(60, 12));
        let popup = ed
            .draw_autocomplete_popup_surface(60, 10)
            .expect("a popup surface while showing with items");
        let popup_row = (0..popup.size.height)
            .map(|r| row_text(&popup, r))
            .find(|t| t.contains("src/main.rs"))
            .unwrap_or_else(|| panic!("expected a popup row with `src/main.rs`"));

        // Single column: the full path (the item's `description`) is drawn at
        // `padding_x` (column 0 here) and there is no second filename column,
        // so the row is exactly the path once trailing band fill is trimmed.
        assert_eq!(popup_row.find("src/main.rs"), Some(0));
        assert_eq!(popup_row.trim_end(), "src/main.rs");
    }

    #[tokio::test]
    async fn popup_is_an_overlay_and_never_grows_the_editor_surface() {
        let mut ed = editor();
        ed.set_autocomplete_provider(Arc::new(MockProvider {
            get: |lines, _l, col, _force| {
                let before = &lines[0][..col];
                if !before.contains('@') {
                    return None;
                }
                Some((
                    vec![
                        AutocompleteItem::new("@a.rs", "a.rs").with_description("a.rs"),
                        AutocompleteItem::new("@b.rs", "b.rs").with_description("b.rs"),
                    ],
                    "@".to_string(),
                ))
            },
        }));

        send(&mut ed, &char_key('@'));
        wait_autocomplete(&mut ed).await;
        assert!(ed.is_showing_autocomplete());

        // With the popup open, the editor surface is still just the input block:
        // one content row plus the two border rules, and `drawn_height` agrees.
        let surf = ed.draw(&ctx(40, 12));
        assert_eq!(surf.size.height, 3, "editor surface is input + two borders");
        assert_eq!(ed.drawn_height(), surf.size.height);

        // Row 0 is the top border rule, not a popup row.
        assert!(
            row_text(&surf, 0).chars().all(|c| c == '─'),
            "row 0 is the top border, not a popup row",
        );
        // The single content row carries the typed `@`, and the caret sits on
        // it with no popup shift.
        let input_row = row_text(&surf, 1);
        assert!(input_row.starts_with('@'), "input row: {input_row:?}");
        let cursor = surf.cursor.expect("caret is reported while focused");
        assert_eq!(cursor.row, 1);

        // The overlay surface holds the popup, sized to the item count.
        let popup = ed
            .draw_autocomplete_popup_surface(40, 10)
            .expect("a popup surface while showing with items");
        assert_eq!(popup.size.height, 2, "two items, both within the window");
    }

    #[tokio::test]
    async fn popup_surface_clamps_rows_and_keeps_the_selection_visible() {
        let items: Vec<AutocompleteItem> = (0..10)
            .map(|n| {
                let path = format!("f{n}.rs");
                AutocompleteItem::new(format!("@{path}"), path.clone()).with_description(path)
            })
            .collect();
        let mut ed = editor();
        // Allow the full ten-row window before the host clamp applies.
        ed.set_autocomplete_max_visible(10);
        ed.set_autocomplete_provider(Arc::new(MockProvider {
            get: move |lines, _l, col, _force| {
                let before = &lines[0][..col];
                if !before.contains('@') {
                    return None;
                }
                Some((items.clone(), "@".to_string()))
            },
        }));

        send(&mut ed, &char_key('@'));
        wait_autocomplete(&mut ed).await;
        assert!(ed.is_showing_autocomplete());
        let _ = ed.draw(&ctx(40, 12));

        // `max_rows` clamps the row count below the item count and below
        // `max_visible`.
        assert_eq!(ed.autocomplete_popup_rows(4), 4);
        let popup = ed
            .draw_autocomplete_popup_surface(40, 4)
            .expect("a clamped popup surface");
        assert_eq!(popup.size.height, 4);

        // Move the selection to the bottom of the list. With a four-row window
        // the recenter must scroll so the selected row stays on screen.
        for _ in 0..9 {
            send(&mut ed, &down());
        }
        let popup = ed
            .draw_autocomplete_popup_surface(40, 4)
            .expect("a clamped popup surface");
        let selected = (0..popup.size.height)
            .map(|r| row_text(&popup, r))
            .any(|t| t.trim_end() == "f9.rs");
        assert!(
            selected,
            "the selected last item stays visible in the clamped window",
        );
    }

    #[tokio::test]
    async fn popup_directory_item_renders_with_a_trailing_slash() {
        let mut ed = editor();
        ed.set_autocomplete_provider(Arc::new(MockProvider {
            get: |lines, _l, col, _force| {
                let before = &lines[0][..col];
                if !before.contains('@') {
                    return None;
                }
                // A fuzzy directory item's `label` ends in `/` while its
                // `description` (the path) does not carry the slash.
                Some((
                    vec![AutocompleteItem::new("@src/", "src/").with_description("src")],
                    "@".to_string(),
                ))
            },
        }));

        send(&mut ed, &char_key('@'));
        wait_autocomplete(&mut ed).await;
        assert!(ed.is_showing_autocomplete());

        let _ = ed.draw(&ctx(60, 12));
        let popup = ed
            .draw_autocomplete_popup_surface(60, 10)
            .expect("a popup surface while showing with items");
        let dir_row = (0..popup.size.height)
            .map(|r| row_text(&popup, r))
            .find(|t| t.trim_end() == "src/")
            .unwrap_or_else(|| panic!("expected a dir row rendered as `src/`"));
        assert_eq!(dir_row.find("src/"), Some(0));
    }

    #[tokio::test]
    async fn popup_scrolls_when_selection_moves_past_the_window() {
        let items: Vec<AutocompleteItem> = (0..10)
            .map(|n| {
                let path = format!("f{n}.rs");
                AutocompleteItem::new(format!("@{path}"), path.clone()).with_description(path)
            })
            .collect();
        let mut ed = editor();
        ed.set_autocomplete_provider(Arc::new(MockProvider {
            get: move |lines, _l, col, _force| {
                let before = &lines[0][..col];
                if !before.contains('@') {
                    return None;
                }
                Some((items.clone(), "@".to_string()))
            },
        }));

        send(&mut ed, &char_key('@'));
        wait_autocomplete(&mut ed).await;
        assert!(ed.is_showing_autocomplete());

        // The list exceeds the visible window, which starts pinned at the top.
        // `usize::MAX` for `max_rows` means the host imposes no extra clamp, so
        // the window is `min(max_visible, len)`.
        let (start_before, count) = ed.autocomplete_popup_window(usize::MAX);
        assert_eq!(start_before, 0);
        assert!(count < 10, "the list must exceed the window to scroll");

        // Moving the selection down past the bottom of the window advances the
        // recenter window's start (Down and Ctrl+N share this path).
        for _ in 0..8 {
            send(&mut ed, &down());
        }
        let (start_after, _) = ed.autocomplete_popup_window(usize::MAX);
        assert!(
            start_after > start_before,
            "the popup window scrolled: {start_before} -> {start_after}",
        );
    }
}
