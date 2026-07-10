//! The transcript view: a scrolling `ListView` over the active
//! transcript's entries.
//!
//! One list item per [`Entry`], built on demand from the shared
//! [`ChatState`] via `Source::Builder`, so a long transcript only
//! materializes the visible rows each frame. The per-entry widget
//! builder ([`build_entry_widget`]) is shared with the sub-agent
//! box, which lays the same widgets out inside its own frame.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use aj_agent::events::AgentId;
use aj_agent::tool::{
    BashStreamTruncation, TaskStatus, TodoPriority, TodoStatus, ToolDetails, TruncationCause,
};
use aj_app::chat::{
    AssistantEntry, ChatState, CompactionEntry, Entry, EntryId, EntryKind, NoticeLevel,
    SubAgentEntry, SubAgentStatus, ToolEntry, ToolStatus, UserEntry,
};
use aj_app::footer::format_tokens;
use aj_app::keybindings::{ACTION_COPY_MESSAGE, default_action_shortcut};
use aj_app::markdown::{Emphasis, RenderOpts};
use aj_app::theme::{ColorMode, Theme, ThemeBg, ThemeColor, ThemeRgb, rgb_to_256};
use aj_models::types::AssistantContent;
use aj_tools::sanitize_terminal_output;
use serde_json::Value;
use vaxis::cell::{Cell, Character, Color, Style};
use vaxis::gwidth;
use vaxis::key::{Key, Modifiers};
use vaxis::mouse;
use vaxis::vxfw::{
    Builder, DrawContext, Event, EventContext, ListView, MaxSize, RelativePoint, RichText,
    ScrollBars, Size, Source, SubSurface, Surface, TextSpan, Widget, WidgetRef,
};

use crate::bubble::{Bubble, BubbleBorder};
use crate::markdown_view::{MarkdownSegment, MarkdownStyles, MarkdownView};
use crate::subagent_box::{SubAgentBox, build_subagent_box, surface_rows};
use crate::terminal::TERMINAL_HYPERLINKS;
use crate::tool_cell::{EXPAND_KEY_LABEL, HintKind, build_tool_cell, expand_hint};

/// Pre-resolved vaxis styles for the transcript's row kinds. The
/// status chrome (loader, footer, pending box) shares the same
/// palette, so its widgets hold a clone too.
///
/// Resolved once from the theme at construction. `Rgb` values are
/// downsampled to the 256-color palette when the theme's
/// [`ColorMode`] is `Color256` (vaxis emits RGB SGR unconditionally
/// and never downsamples, so the frontend does it here). A theme swap
/// rebuilds the whole struct, see [`TranscriptView::set_styles`] and
/// the shell's re-style path.
pub(crate) struct TranscriptStyles {
    pub(crate) text: Style,
    pub(crate) user: Style,
    pub(crate) thinking: Style,
    pub(crate) dim: Style,
    /// Gray tint for the chat scrollbar thumb. This is aj-next chrome with no
    /// `aj` counterpart (aj has no in-app scrollbar), so it stays a concrete
    /// gray rather than the faint attribute `dim` carries.
    pub(crate) scrollbar_thumb: Style,
    pub(crate) warning: Style,
    pub(crate) error: Style,
    pub(crate) success: Style,
    /// The theme's primary accent, where `aj` uses its cyan (the
    /// loader spinner, the pending box's kind label).
    pub(crate) accent: Style,
    /// Bold tool name in a tool cell's header.
    pub(crate) bold: Style,
    /// Bold key label in the keybinding-hint palette color (`keybindingHint`),
    /// used by the splash's `{key} for commands` hint (and, per Spec E-10, the
    /// list-row shortcut column).
    pub(crate) keybinding_hint: Style,
    pub(crate) diff_add: Style,
    pub(crate) diff_remove: Style,
    /// Tool-bubble tints per visual status.
    pub(crate) tool_pending_bg: Color,
    pub(crate) tool_success_bg: Color,
    pub(crate) tool_error_bg: Color,
    /// The user-message bubble tint.
    pub(crate) user_message_bg: Color,
    /// Border glyph color for the focused user message's marker in
    /// transcript-focus mode, the theme's `borderAccent` (Spec E section 2).
    pub(crate) border_accent: Color,
    /// Background tint for a transcript selection's highlight, the app's
    /// selection color (see Spec E section 2). Only the background is
    /// restyled over the composed frame, so the selected text stays readable.
    pub(crate) selection_bg: Color,
    /// Foreground mapper for markdown span roles, consumed by
    /// [`MarkdownView`]. Rebuilt from the theme here so a runtime swap
    /// re-tints markdown through the same `set_styles` path.
    pub(crate) markdown: MarkdownStyles,
    /// Whether markdown links emit OSC-8 hyperlinks. See
    /// [`TERMINAL_HYPERLINKS`](crate::terminal::TERMINAL_HYPERLINKS).
    pub(crate) hyperlinks: bool,
}

/// The SGR-2 faint attribute over the default foreground: the exact analogue of
/// `aj-tui`'s `style::dim`, which every dim transcript row, tool-cell detail, and
/// background-task line uses. It is an attribute, not the `Dim` palette gray
/// (`#666666`), so it tracks the terminal's own foreground the way `aj` does.
pub(crate) fn faint() -> Style {
    Style {
        dim: true,
        ..Style::default()
    }
}

impl TranscriptStyles {
    pub(crate) fn from_theme(theme: &Theme) -> TranscriptStyles {
        let mode = theme.color_mode();
        let fg = |token: ThemeColor| Style {
            fg: vaxis_color(theme.fg_color(token), mode),
            ..Style::default()
        };
        let bg = |token: ThemeBg| vaxis_color(theme.bg_color(token), mode);
        TranscriptStyles {
            text: fg(ThemeColor::Text),
            user: fg(ThemeColor::UserMessageText),
            thinking: Style {
                italic: true,
                ..fg(ThemeColor::ThinkingText)
            },
            dim: faint(),
            scrollbar_thumb: fg(ThemeColor::Dim),
            warning: fg(ThemeColor::Warning),
            error: fg(ThemeColor::Error),
            success: fg(ThemeColor::Success),
            accent: fg(ThemeColor::Accent),
            bold: Style {
                bold: true,
                ..fg(ThemeColor::ToolTitle)
            },
            keybinding_hint: Style {
                bold: true,
                ..fg(ThemeColor::KeybindingHint)
            },
            diff_add: fg(ThemeColor::ToolDiffAdded),
            diff_remove: fg(ThemeColor::ToolDiffRemoved),
            tool_pending_bg: bg(ThemeBg::ToolPendingBg),
            tool_success_bg: bg(ThemeBg::ToolSuccessBg),
            tool_error_bg: bg(ThemeBg::ToolErrorBg),
            user_message_bg: bg(ThemeBg::UserMessageBg),
            border_accent: vaxis_color(theme.fg_color(ThemeColor::BorderAccent), mode),
            selection_bg: bg(ThemeBg::SelectedBg),
            markdown: MarkdownStyles::from_theme(theme),
            hyperlinks: TERMINAL_HYPERLINKS,
        }
    }
}

/// Map a theme palette value onto a vaxis color, downsampling to the
/// 256-color palette when `mode` is `Color256`.
///
/// vaxis writes an `Rgb` color as a truecolor SGR sequence with no
/// downsampling of its own, so a limited terminal would see garbled
/// colors. We fold the same `rgb_to_256` mapping the `aj` frontend
/// uses into a palette index here, so both frontends render a
/// `Color256` theme identically.
pub(crate) fn vaxis_color(rgb: ThemeRgb, mode: ColorMode) -> Color {
    match rgb {
        ThemeRgb::Rgb(r, g, b) => match mode {
            ColorMode::Truecolor => Color::Rgb([r, g, b]),
            ColorMode::Color256 => Color::Index(rgb_to_256(r, g, b)),
        },
        ThemeRgb::Ansi256(i) => Color::Index(i),
        ThemeRgb::Default => Color::Default,
    }
}

/// Source-line count shown for a collapsible user message (a harness
/// task notification) while collapsed. A short preview keeps a long
/// notification from flooding the scrollback while still surfacing
/// its first (and most informative) line.
const USER_COLLAPSED_LINES: usize = 10;

/// Upper bound on cached slots. The live working set is the viewport (a
/// screen's worth of entries), so this sits comfortably above it. Overflow
/// evicts the least-recently-used slot, which is correctness-neutral: a
/// dropped slot simply rebuilds on its next draw. Bounding the map keeps a
/// long append-only session from growing it without limit.
const ENTRY_CACHE_CAPACITY: usize = 512;

/// Rows kept in common between two page-scroll steps, so a reader keeps a
/// little context across a page turn rather than jumping a full viewport.
const PAGE_OVERLAP: u16 = 2;

/// Page size (in lines) used before the first draw has measured the real
/// viewport height. A page-scroll issued that early is rare, so a sane
/// constant is enough until the next draw records the true height.
const DEFAULT_PAGE_LINES: i32 = 20;

/// One cached per-entry render: the drawn surface plus the
/// `(fingerprint, width)` it was drawn for. A lookup hits only when both
/// match the live entry, so a slot never serves stale content.
struct CachedEntry {
    fingerprint: u64,
    width: u16,
    surface: Surface,
    /// Access tick of the last lookup that touched this slot, for LRU
    /// eviction.
    last_used: u64,
}

/// Memoizes drawn per-entry surfaces keyed by `(active view, entry id)`.
///
/// Owned by [`TranscriptView`] and shared into the [`EntryBuilder`] by
/// `Rc<RefCell<..>>`. One slot per key, so stale `(fingerprint, width)`
/// variants never accumulate. Session-wide render inputs (the theme,
/// `tools_expanded`, `hide_thinking_block`, `syntax_highlight`, the active
/// view) are handled by clearing the whole cache when they change rather than
/// folding them into every fingerprint (see [`TranscriptView::draw`] and
/// [`TranscriptView::set_styles`]). Width is a per-slot key.
///
/// Storing surfaces is safe today: no transcript entry participates in event
/// dispatch, so replaying a stored surface (whose `widget` stamp is `None`,
/// then re-stamped onto the outer [`CachingEntry`] by `draw_widget`) does not
/// break hit-testing.
///
/// Transcript-focus mode does not change this. The focused user message's
/// marker is a border painted into the bubble's own padding (Spec E section
/// 2), so it is per-entry render output folded into that entry's fingerprint,
/// not a `ListView`-level gutter. Entries stay non-interactive and the "no
/// interactive entries" assumption still holds.
struct EntryRenderCache {
    slots: HashMap<(AgentId, EntryId), CachedEntry>,
    /// Monotonic lookup counter stamped onto a slot's `last_used` on every
    /// access, so eviction can drop the coldest slot.
    tick: u64,
    /// Effectiveness/correctness instrumentation asserted by tests: a hit
    /// replayed a stored surface, a miss rebuilt it.
    hits: u64,
    misses: u64,
}

impl EntryRenderCache {
    fn new() -> EntryRenderCache {
        EntryRenderCache {
            slots: HashMap::new(),
            tick: 0,
            hits: 0,
            misses: 0,
        }
    }

    /// Drop every cached surface. Called when a session-wide render input
    /// changes (theme swap, a display toggle, a view switch), since those are
    /// not part of any per-entry fingerprint.
    fn clear(&mut self) {
        self.slots.clear();
    }

    /// The cached surface for `key` when the slot's fingerprint and width both
    /// match (a HIT). Otherwise `None` (a MISS), and the caller rebuilds and
    /// calls [`insert`](Self::insert).
    fn get(&mut self, key: (AgentId, EntryId), fingerprint: u64, width: u16) -> Option<Surface> {
        self.tick += 1;
        let tick = self.tick;
        match self.slots.get_mut(&key) {
            Some(slot) if slot.fingerprint == fingerprint && slot.width == width => {
                slot.last_used = tick;
                self.hits += 1;
                Some(slot.surface.clone())
            }
            _ => {
                self.misses += 1;
                None
            }
        }
    }

    /// Store `surface` for `key` under `(fingerprint, width)`, replacing any
    /// prior slot for the key, and evict the coldest slot when over capacity.
    fn insert(&mut self, key: (AgentId, EntryId), fingerprint: u64, width: u16, surface: Surface) {
        let last_used = self.tick;
        self.slots.insert(
            key,
            CachedEntry {
                fingerprint,
                width,
                surface,
                last_used,
            },
        );
        if self.slots.len() > ENTRY_CACHE_CAPACITY {
            self.evict_coldest();
        }
    }

    /// Remove the least-recently-used slot. O(n) over the map, but only fires
    /// on an insert past capacity, and the map is bounded.
    fn evict_coldest(&mut self) {
        if let Some(key) = self
            .slots
            .iter()
            .min_by_key(|(_, slot)| slot.last_used)
            .map(|(key, _)| *key)
        {
            self.slots.remove(&key);
        }
    }
}

/// One cached per-entry text render: the entry's rendered rows plus the
/// `(fingerprint, width)` they were laid out for. A lookup hits only when both
/// match the live entry, so a slot never serves stale rows.
struct EntryTextSlot {
    fingerprint: u64,
    width: u16,
    rows: Rc<Vec<Vec<Cell>>>,
    /// Access tick of the last lookup that touched this slot, for LRU
    /// eviction.
    last_used: u64,
}

/// Memoizes the rendered rows (cells) of the active view's entries, keyed by
/// [`EntryId`] and validated by `(fingerprint, width)`.
///
/// Backs select-to-copy: extraction and the highlight lay out only the entries
/// a selection spans, on demand, and reuse the rows while the entry's
/// fingerprint and width hold. Owned as a plain field on [`TranscriptView`]
/// (not shared into the builder), so it needs no interior mutability. One slot
/// per entry, bounded by [`ENTRY_CACHE_CAPACITY`] with LRU eviction, so a long
/// append-only session cannot grow it without limit.
struct EntryTextCache {
    slots: HashMap<EntryId, EntryTextSlot>,
    /// Monotonic lookup counter stamped onto a slot's `last_used` on every
    /// access, so eviction can drop the coldest slot.
    tick: u64,
}

impl EntryTextCache {
    fn new() -> EntryTextCache {
        EntryTextCache {
            slots: HashMap::new(),
            tick: 0,
        }
    }

    /// Drop every cached row set. Called when a session-wide render input
    /// changes (theme swap, a display toggle, a view switch, a session
    /// rebuild), since those are not part of any per-entry fingerprint. The
    /// view switch is what keeps the `EntryId`-only key safe: two views restart
    /// ids at 0, so their slots would collide without the clear.
    fn clear(&mut self) {
        self.slots.clear();
    }

    /// The cached rows for `id` when the slot's fingerprint and width both
    /// match, else `None` (the caller then lays the entry out and inserts).
    fn get(&mut self, id: EntryId, fingerprint: u64, width: u16) -> Option<Rc<Vec<Vec<Cell>>>> {
        self.tick += 1;
        let tick = self.tick;
        match self.slots.get_mut(&id) {
            Some(slot) if slot.fingerprint == fingerprint && slot.width == width => {
                slot.last_used = tick;
                Some(Rc::clone(&slot.rows))
            }
            _ => None,
        }
    }

    /// Store `rows` for `id` under `(fingerprint, width)`, replacing any prior
    /// slot for the id, and evict the coldest slot when over capacity.
    fn insert(&mut self, id: EntryId, fingerprint: u64, width: u16, rows: Rc<Vec<Vec<Cell>>>) {
        let last_used = self.tick;
        self.slots.insert(
            id,
            EntryTextSlot {
                fingerprint,
                width,
                rows,
                last_used,
            },
        );
        if self.slots.len() > ENTRY_CACHE_CAPACITY {
            self.evict_coldest();
        }
    }

    fn evict_coldest(&mut self) {
        if let Some(id) = self
            .slots
            .iter()
            .min_by_key(|(_, slot)| slot.last_used)
            .map(|(id, _)| *id)
        {
            self.slots.remove(&id);
        }
    }
}

/// Lazily builds one row widget per transcript entry of the active view.
/// Shared with the [`ListView`] it feeds.
///
/// The widget it returns is a [`CachingEntry`]: `item_at_idx` computes only
/// the cheap [`entry_fingerprint`] here (it has the `ChatState` borrow but not
/// the draw width) and defers the real build+draw to the wrapper's `draw`,
/// which has the width and only rebuilds on a cache miss.
struct EntryBuilder {
    chat: Rc<RefCell<ChatState>>,
    styles: Rc<TranscriptStyles>,
    cache: Rc<RefCell<EntryRenderCache>>,
    /// The transcript-focus flag, shared with [`TranscriptView`] and the
    /// keymap host context. Read live so the focus border tracks the current
    /// mode. The transcript is the single writer.
    focus_mode: Rc<std::cell::Cell<bool>>,
    /// The pre-styled copy-key hint (`y to copy`) the focused bubble's border
    /// shows, resolved once through the keybinding data. Shared by `Rc` so
    /// each `CachingEntry` clones a handle rather than the spans.
    copy_label: Rc<Vec<TextSpan>>,
}

impl Builder for EntryBuilder {
    fn item_at_idx(&self, idx: usize, cursor: usize) -> Option<WidgetRef> {
        let chat = self.chat.borrow();
        let agent = chat.active_view();
        let entry = chat.transcript(agent)?.entries().get(idx)?;
        // The focus border is per-cursor chrome, not entry content, so fold it
        // into the fingerprint. Without this the cache would replay a stale
        // bordered or unbordered surface when focus moves. Folding `focused`
        // in re-renders exactly the entry gaining and the entry losing focus
        // and leaves the rest cache hits.
        let focused =
            self.focus_mode.get() && idx == cursor && matches!(entry.kind, EntryKind::User(_));
        let mut hasher = DefaultHasher::new();
        fingerprint_into(entry, &chat, &mut hasher);
        focused.hash(&mut hasher);
        let fingerprint = hasher.finish();
        Some(Rc::new(RefCell::new(CachingEntry {
            cache: Rc::clone(&self.cache),
            chat: Rc::clone(&self.chat),
            styles: Rc::clone(&self.styles),
            agent,
            entry_id: entry.id,
            fingerprint,
            focused,
            copy_label: Rc::clone(&self.copy_label),
        })))
    }
}

/// A per-entry list item that memoizes its drawn surface in the shared
/// [`EntryRenderCache`].
///
/// The cache key needs the draw width, which `item_at_idx` does not have, so
/// the builder hands back one of these carrying the pre-computed fingerprint
/// and the `Rc` handles. The real build+draw happens here, in `draw`, and only
/// on a cache miss, so a hit skips both.
struct CachingEntry {
    cache: Rc<RefCell<EntryRenderCache>>,
    chat: Rc<RefCell<ChatState>>,
    styles: Rc<TranscriptStyles>,
    agent: AgentId,
    entry_id: EntryId,
    fingerprint: u64,
    /// Whether this entry is the focused user message, so its bubble gets the
    /// focus border on the miss-path build. Already folded into `fingerprint`.
    focused: bool,
    /// The copy-key hint shown on the border's bottom edge, used only when
    /// `focused`.
    copy_label: Rc<Vec<TextSpan>>,
}

impl Widget for CachingEntry {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let width = ctx.max.width.unwrap_or(ctx.min.width);
        let key = (self.agent, self.entry_id);

        // HIT: the stored surface was drawn for this fingerprint and width, so
        // replay it verbatim. Bind the lookup to a `let` so the cache's
        // `RefMut` is released before the miss path re-borrows it.
        let cached = self.cache.borrow_mut().get(key, self.fingerprint, width);
        if let Some(surface) = cached {
            return surface;
        }

        // MISS: rebuild the real widget and draw it. The `item_at_idx` chat
        // borrow is already dropped (draw_builder resolves the WidgetRef, then
        // draw_widget draws it), so this re-borrow never overlaps.
        let surface = {
            let chat = self.chat.borrow();
            let Some(entry) = chat
                .transcript(self.agent)
                .and_then(|t| t.get(self.entry_id))
            else {
                // Append-only transcripts mean the entry can't vanish; return
                // an empty surface defensively rather than panic.
                return Surface::empty();
            };
            // The focus border is threaded only when this entry is the focused
            // user message. Every other entry builds unbordered.
            let focus = self.focused.then(|| self.copy_label.as_slice());
            let mut widget =
                build_entry_widget(entry, &chat, &self.styles, false, focus).into_boxed();
            widget.draw(ctx)
        };
        self.cache
            .borrow_mut()
            .insert(key, self.fingerprint, width, surface.clone());
        surface
    }
}

/// A layout-free hash of the cheap per-entry fields that change an entry's
/// rendered surface. The render cache keys a slot's validity on this: if the
/// fingerprint (and the draw width) match the live entry, the stored surface
/// is replayed instead of rebuilt.
///
/// Philosophy: over-fingerprint. A field we forget shows stale content (a real
/// bug); a field we include that doesn't affect rendering only costs a
/// harmless rebuild. Session-wide render inputs (`tools_expanded`,
/// `hide_thinking_block`, `syntax_highlight`, the active view, the theme, the
/// draw width) are NOT hashed here: the cache clears wholesale when they
/// change, and width is a per-slot key.
fn entry_fingerprint(entry: &Entry, chat: &ChatState) -> u64 {
    let mut hasher = DefaultHasher::new();
    fingerprint_into(entry, chat, &mut hasher);
    hasher.finish()
}

/// Fold `entry`'s render-affecting fields into `hasher`. Split out from
/// [`entry_fingerprint`] so the sub-agent arm can recurse into its child
/// entries with the same logic.
fn fingerprint_into(entry: &Entry, chat: &ChatState, hasher: &mut DefaultHasher) {
    // A per-kind tag so identical numeric payloads across kinds never collide.
    match &entry.kind {
        EntryKind::Assistant(a) => {
            0u8.hash(hasher);
            assistant_fingerprint(a, hasher);
        }
        EntryKind::Tool(t) => {
            1u8.hash(hasher);
            tool_fingerprint(t, chat, hasher);
        }
        EntryKind::User(u) => {
            2u8.hash(hasher);
            u.joined_text().len().hash(hasher);
            u.collapsible.hash(hasher);
        }
        EntryKind::SubAgent(s) => {
            3u8.hash(hasher);
            subagent_fingerprint(s, chat, hasher);
        }
        EntryKind::Compaction(c) => {
            4u8.hash(hasher);
            c.summary.len().hash(hasher);
            c.tokens_before.hash(hasher);
            c.tokens_after.hash(hasher);
        }
        // Notice and turn-usage rows are immutable after append, so the id and
        // width already distinguish them. We fold a stable discriminant (plus
        // a couple of trivially cheap fields) for defence in depth.
        EntryKind::Notice(n) => {
            5u8.hash(hasher);
            notice_level_tag(n.level).hash(hasher);
            n.text.len().hash(hasher);
        }
        EntryKind::TurnUsage(_) => {
            6u8.hash(hasher);
        }
    }
}

/// Assistant / reasoning fields: the content-block count, a per-block tag (so
/// a block changing kind is caught), the summed text and thinking byte
/// lengths, the thinking `redacted` flag (which flips the placeholder text
/// without changing its length), and `finalized`. `hide_thinking_block` is a
/// global clear, so it is not folded in.
fn assistant_fingerprint(a: &AssistantEntry, hasher: &mut DefaultHasher) {
    a.message.content.len().hash(hasher);
    let mut text_len = 0usize;
    let mut thinking_len = 0usize;
    for block in &a.message.content {
        match block {
            AssistantContent::Text(t) => {
                0u8.hash(hasher);
                text_len += t.text.len();
            }
            AssistantContent::Thinking(t) => {
                1u8.hash(hasher);
                t.redacted.hash(hasher);
                thinking_len += t.thinking.len();
            }
            AssistantContent::ToolCall(_) => 2u8.hash(hasher),
        }
    }
    text_len.hash(hasher);
    thinking_len.hash(hasher);
    a.finalized.hash(hasher);
}

/// Tool-cell fields: the status (plus `is_error`), `header_only`, the details
/// presence, the details variant discriminant with a per-variant size proxy,
/// and the badge task's live status (which drives the badge text and the cell
/// tint and changes over the cell's life). `tools_expanded` is a global clear.
///
/// The header also renders `entry.tool` and `entry.args`, and the badge gating
/// reads the task's `kind`. None are folded in: all three are fixed when the
/// cell is appended (`ToolExecutionStart` carries the fully-validated args, and
/// a task's `kind` is set at `TaskStart`) and never mutate for a given entry
/// id, so the first miss captures them for good. If a future change lets tool
/// args stream into an existing cell, they must move into the fingerprint.
fn tool_fingerprint(t: &ToolEntry, chat: &ChatState, hasher: &mut DefaultHasher) {
    match t.status {
        ToolStatus::Running => 0u8.hash(hasher),
        ToolStatus::Done { is_error } => {
            1u8.hash(hasher);
            is_error.hash(hasher);
        }
    }
    t.header_only.hash(hasher);
    match &t.details {
        None => 0u8.hash(hasher),
        Some(details) => {
            1u8.hash(hasher);
            details_fingerprint(details, hasher);
        }
    }
    // Mirror `tool_cell::badge_task_id`: the persisted Bash `task_id` wins,
    // else the live `entry.task`. The badge and tint follow that task's status.
    let badge_id = match &t.details {
        Some(ToolDetails::Bash {
            task_id: Some(id), ..
        }) => Some(*id),
        _ => t.task,
    };
    badge_id.hash(hasher);
    if let Some(id) = badge_id {
        let status = chat.tasks().get(&id).map(|info| info.status);
        task_status_fingerprint(status, hasher);
    }
}

/// The details variant discriminant plus a cheap, layout-free size proxy. For
/// the streaming variants (bash streams, text/report bodies) the payload only
/// ever grows, so a length proxy reliably changes as content arrives. `Json`
/// hashes its structure since it is the escape hatch and not append-only.
fn details_fingerprint(details: &ToolDetails, hasher: &mut DefaultHasher) {
    match details {
        ToolDetails::Text { summary, body } => {
            0u8.hash(hasher);
            summary.len().hash(hasher);
            body.len().hash(hasher);
        }
        ToolDetails::Diff {
            path,
            before,
            after,
        } => {
            1u8.hash(hasher);
            path.len().hash(hasher);
            before.len().hash(hasher);
            after.len().hash(hasher);
        }
        ToolDetails::Bash {
            command,
            stdout,
            stderr,
            exit_code,
            truncated,
            full_output_path,
            stdout_truncation,
            stderr_truncation,
            task_id,
        } => {
            2u8.hash(hasher);
            command.len().hash(hasher);
            stdout.len().hash(hasher);
            stderr.len().hash(hasher);
            exit_code.hash(hasher);
            truncated.hash(hasher);
            full_output_path
                .as_ref()
                .map(|p| p.as_os_str().len())
                .hash(hasher);
            truncation_fingerprint(stdout_truncation, hasher);
            truncation_fingerprint(stderr_truncation, hasher);
            task_id.hash(hasher);
        }
        ToolDetails::SubAgentReport {
            agent_id,
            task,
            report,
        } => {
            3u8.hash(hasher);
            agent_id.hash(hasher);
            task.len().hash(hasher);
            report.len().hash(hasher);
        }
        ToolDetails::Todos { items } => {
            4u8.hash(hasher);
            items.len().hash(hasher);
            for item in items {
                item.content.len().hash(hasher);
                todo_status_tag(item.status).hash(hasher);
                todo_priority_tag(item.priority).hash(hasher);
            }
        }
        ToolDetails::Image {
            summary,
            mime_type,
            original_dimensions,
            displayed_dimensions,
        } => {
            5u8.hash(hasher);
            summary.len().hash(hasher);
            mime_type.len().hash(hasher);
            original_dimensions.hash(hasher);
            displayed_dimensions.hash(hasher);
        }
        ToolDetails::Json(value) => {
            6u8.hash(hasher);
            json_fingerprint(value, hasher);
        }
    }
}

/// A per-stream truncation summary's presence plus the numeric fields that
/// feed its marker line.
fn truncation_fingerprint(t: &Option<BashStreamTruncation>, hasher: &mut DefaultHasher) {
    match t {
        None => 0u8.hash(hasher),
        Some(t) => {
            1u8.hash(hasher);
            t.total_lines.hash(hasher);
            t.total_bytes.hash(hasher);
            t.output_lines.hash(hasher);
            t.output_bytes.hash(hasher);
            t.last_line_partial.hash(hasher);
            t.last_line_bytes.hash(hasher);
            match t.truncated_by {
                TruncationCause::Lines => 0u8,
                TruncationCause::Bytes => 1u8,
            }
            .hash(hasher);
        }
    }
}

/// Structure-only hash of a JSON value: the variant tag, the scalar value (for
/// number/bool/string, so `1` and `2` don't collide), and container lengths,
/// recursing into children. Layout-free and allocation-light.
fn json_fingerprint(value: &Value, hasher: &mut DefaultHasher) {
    match value {
        Value::Null => 0u8.hash(hasher),
        Value::Bool(b) => {
            1u8.hash(hasher);
            b.hash(hasher);
        }
        Value::Number(n) => {
            2u8.hash(hasher);
            n.to_string().hash(hasher);
        }
        Value::String(s) => {
            3u8.hash(hasher);
            s.hash(hasher);
        }
        Value::Array(items) => {
            4u8.hash(hasher);
            items.len().hash(hasher);
            for item in items {
                json_fingerprint(item, hasher);
            }
        }
        Value::Object(map) => {
            5u8.hash(hasher);
            map.len().hash(hasher);
            for (key, val) in map {
                key.hash(hasher);
                json_fingerprint(val, hasher);
            }
        }
    }
}

/// Sub-agent box fields: its run status, the child transcript's entry count,
/// and the fold of every child entry's fingerprint. Folding ALL children (not
/// just the tail) is required: a background task can update a non-tail child
/// cell. This is O(child entries) but layout-free, far cheaper than building
/// and drawing them.
///
/// The box title also renders `s.child` and `s.task`, neither folded in: both
/// are fixed at `SubAgentStart` and never mutate, so the first miss captures
/// them. `s.report` is not rendered by the box (the parent's tool cell carries
/// it), so it needs no coverage here.
fn subagent_fingerprint(s: &SubAgentEntry, chat: &ChatState, hasher: &mut DefaultHasher) {
    match s.status {
        SubAgentStatus::Running => 0u8.hash(hasher),
        SubAgentStatus::Done => 1u8.hash(hasher),
    }
    match chat.transcript(AgentId::Sub(s.child)) {
        Some(t) => {
            t.entries().len().hash(hasher);
            for child in t.entries() {
                fingerprint_into(child, chat, hasher);
            }
        }
        None => 0usize.hash(hasher),
    }
}

fn notice_level_tag(level: NoticeLevel) -> u8 {
    match level {
        NoticeLevel::Info => 0,
        NoticeLevel::Warning => 1,
        NoticeLevel::Error => 2,
    }
}

fn task_status_fingerprint(status: Option<TaskStatus>, hasher: &mut DefaultHasher) {
    match status {
        None => 0u8.hash(hasher),
        Some(TaskStatus::Running) => 1u8.hash(hasher),
        Some(TaskStatus::Killed) => 2u8.hash(hasher),
        Some(TaskStatus::Exited(code)) => {
            // The exit code drives the badge text and the cell tint, so fold
            // it in. `Option<i32>` hashes directly.
            3u8.hash(hasher);
            code.hash(hasher);
        }
    }
}

fn todo_status_tag(status: TodoStatus) -> u8 {
    match status {
        TodoStatus::Todo => 0,
        TodoStatus::InProgress => 1,
        TodoStatus::Completed => 2,
    }
}

fn todo_priority_tag(priority: TodoPriority) -> u8 {
    match priority {
        TodoPriority::Low => 0,
        TodoPriority::Medium => 1,
        TodoPriority::High => 2,
    }
}

/// A built per-entry widget. One enum instead of a boxed trait
/// object so the `ListView` path can wrap each concrete type in its
/// own `WidgetRef` (the unsize coercion needs the concrete type).
pub(crate) enum EntryWidget {
    Bubble(Bubble),
    Rich(RichText),
    Markdown(MarkdownView),
    SubAgent(SubAgentBox),
}

impl EntryWidget {
    /// Erase to a boxed widget, for the sub-agent box's child list.
    pub(crate) fn into_boxed(self) -> Box<dyn Widget> {
        match self {
            EntryWidget::Bubble(b) => Box::new(b),
            EntryWidget::Rich(r) => Box::new(r),
            EntryWidget::Markdown(m) => Box::new(m),
            EntryWidget::SubAgent(b) => Box::new(b),
        }
    }
}

/// Build the widget for one transcript entry. Shared between the
/// top-level list and the sub-agent box's inner layout.
///
/// `nested` is true when building inside a sub-agent box: a nested
/// `SubAgent` entry then renders as the dim stub line instead of
/// recursing. Sub-agents can't spawn sub-agents (the `agent` tool is
/// excluded from their tool list), so that arm is defensive only.
///
/// `focus` carries the pre-styled copy-key hint when this entry is the focused
/// user message, marking its bubble with the focus border (Spec E section 2).
/// It is `None` for every other entry and every non-`User` kind ignores it.
pub(crate) fn build_entry_widget(
    entry: &Entry,
    chat: &ChatState,
    styles: &TranscriptStyles,
    nested: bool,
    focus: Option<&[TextSpan]>,
) -> EntryWidget {
    match &entry.kind {
        EntryKind::Tool(tool) => EntryWidget::Bubble(build_tool_cell(
            tool,
            chat.tasks(),
            chat.tools_expanded,
            styles,
        )),
        EntryKind::User(user) => {
            EntryWidget::Bubble(build_user_bubble(user, chat.tools_expanded, styles, focus))
        }
        EntryKind::SubAgent(s) if !nested => {
            EntryWidget::SubAgent(build_subagent_box(s, chat, styles))
        }
        // Assistant prose and the expanded compaction summary render as
        // markdown through the width-aware `MarkdownView`. A nested assistant
        // entry (inside a sub-agent box) takes this path too, so a child's
        // messages render as markdown just like the top-level ones.
        EntryKind::Assistant(a) => EntryWidget::Markdown(build_assistant_markdown(
            a,
            chat.hide_thinking_block,
            chat.syntax_highlight,
            styles,
        )),
        EntryKind::Compaction(c) => EntryWidget::Markdown(build_compaction_markdown(
            c,
            chat.tools_expanded,
            chat.syntax_highlight,
            styles,
        )),
        _ => EntryWidget::Rich(RichText::new(entry_spans(entry, styles))),
    }
}

/// Build the user-message bubble: the full message under the
/// user-message tint, with no `> ` prefix (the tint is the entire
/// visual cue, which also keeps the text cleanly copy-pasteable).
///
/// Harness-injected task notifications (`collapsible`) fold to their
/// first [`USER_COLLAPSED_LINES`] source lines plus an italic expand
/// hint, and expand together with tool output under the session-wide
/// `tools_expanded` flag. Typed prompts always render in full.
///
/// When `focus` is `Some`, the bubble carries the focus border in the
/// `borderAccent` color, with the supplied copy-key hint on its bottom edge
/// (Spec E section 2).
fn build_user_bubble(
    user: &UserEntry,
    expanded: bool,
    styles: &TranscriptStyles,
    focus: Option<&[TextSpan]>,
) -> Bubble {
    let span = |text: String, style: Style| TextSpan {
        text,
        style,
        ..TextSpan::default()
    };
    // Task notifications embed captured task output, so the text is
    // not guaranteed to be terminal-safe the way a typed prompt is.
    let text = sanitize_terminal_output(&user.joined_text());
    let mut lines: Vec<&str> = text.lines().collect();
    let fold = user.collapsible && !expanded && lines.len() > USER_COLLAPSED_LINES;
    let hint = fold.then(|| {
        let more = lines.len() - USER_COLLAPSED_LINES;
        lines.truncate(USER_COLLAPSED_LINES);
        expand_hint(more, HintKind::More)
    });
    let mut spans = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            spans.push(span("\n".into(), styles.user));
        }
        spans.push(span((*line).to_string(), styles.user));
    }
    if let Some(hint) = hint {
        // Italic rather than dim: the hint sits on the bubble tint,
        // where the muted-but-legible cue is the slant (mirroring
        // `aj`'s markdown-emphasis hint).
        spans.push(span("\n".into(), styles.user));
        spans.push(span(
            hint,
            Style {
                italic: true,
                ..styles.text
            },
        ));
    }
    let bubble = Bubble::entry(spans, Some(styles.user_message_bg), styles.text);
    match focus {
        Some(label) => bubble.with_border(BubbleBorder {
            color: styles.border_accent,
            label: label.to_vec(),
        }),
        None => bubble,
    }
}

/// The pre-styled copy-key hint shown on the focused-message border's bottom
/// edge, resolved through the keybinding data so the key is never a literal
/// (Spec E section 2). The key renders in the accent color and the rest muted,
/// the way an overlay styles the key hints in its chrome.
fn copy_label_spans(styles: &TranscriptStyles) -> Vec<TextSpan> {
    let key = default_action_shortcut(ACTION_COPY_MESSAGE).unwrap_or_default();
    vec![
        TextSpan {
            text: key,
            style: styles.accent,
            ..TextSpan::default()
        },
        TextSpan {
            text: " to copy".into(),
            style: styles.dim,
            ..TextSpan::default()
        },
    ]
}

/// Build the styled spans for one entry that renders through [`RichText`]
/// (notices, usage, the defensive nested sub-agent stub), ending in a blank
/// spacer row so consecutive entries don't visually collide.
///
/// Assistant, compaction, tool, and user entries render through their own
/// widgets (see [`build_entry_widget`]); their arms here return no content and
/// exist only to keep the match total.
fn entry_spans(entry: &Entry, styles: &TranscriptStyles) -> Vec<TextSpan> {
    let span = |text: String, style: Style| TextSpan {
        text,
        style,
        ..TextSpan::default()
    };
    let mut spans = match &entry.kind {
        // These render through a dedicated widget: user and tool entries
        // through a bubble, assistant prose and compaction summaries through
        // a `MarkdownView`. The builder never routes them here, so these arms
        // only keep the match total.
        EntryKind::User(_)
        | EntryKind::Assistant(_)
        | EntryKind::Tool(_)
        | EntryKind::Compaction(_) => Vec::new(),
        // The `SubAgent` arm is only reachable as the nested-inside-a-box
        // fallback, which can't occur live (sub-agents don't spawn
        // sub-agents), so a dim stub is enough.
        EntryKind::SubAgent(s) => vec![span(format!("[sub-agent {}]", s.child), styles.dim)],
        // Notice and usage rows carry the same one-column left inset
        // the tool bubbles have, so the transcript's left edge lines
        // up. Wrapped continuation lines start at column zero, which
        // is acceptable for these short rows.
        EntryKind::Notice(n) => {
            let style = match n.level {
                NoticeLevel::Info => styles.dim,
                NoticeLevel::Warning => styles.warning,
                NoticeLevel::Error => styles.error,
            };
            vec![span(format!(" {}", n.text), style)]
        }
        EntryKind::TurnUsage(u) => vec![span(format!(" {}", u.line()), styles.dim)],
    };
    // Normalize away trailing newlines so the spacer below yields
    // exactly one blank row regardless of how the content ends.
    if let Some(last) = spans.last_mut() {
        let trimmed = last.text.trim_end_matches('\n').len();
        last.text.truncate(trimmed);
    }
    // A trailing "\n\n" adds one empty hard line, which the wrap
    // engine renders as a blank spacer row.
    spans.push(span("\n\n".to_string(), styles.text));
    spans
}

/// Build the [`MarkdownView`] for an assistant entry: one segment per content
/// block, in order.
///
/// Plain text renders under the normal text style; thinking blocks under the
/// thinking style (its own color plus italic). Tool calls render as their own
/// `Tool` transcript entries, so the inline block is skipped here to avoid
/// duplicating them. Redacted and (when `hide_thinking`) hidden thinking
/// collapse to their placeholders, matching the plain-text renderer they
/// replace.
fn build_assistant_markdown(
    a: &AssistantEntry,
    hide_thinking: bool,
    syntax_highlight: bool,
    styles: &TranscriptStyles,
) -> MarkdownView {
    let text_opts = RenderOpts {
        hyperlinks: styles.hyperlinks,
        default_emphasis: Emphasis::default(),
        syntax_highlight,
    };
    // Thinking prose renders italic by default, the same emphasis `aj` applies
    // to a thinking block's markdown.
    let thinking_opts = RenderOpts {
        hyperlinks: styles.hyperlinks,
        default_emphasis: Emphasis {
            italic: true,
            ..Emphasis::default()
        },
        syntax_highlight,
    };
    let mut segments = Vec::new();
    for block in &a.message.content {
        let segment = match block {
            AssistantContent::Text(t) => MarkdownSegment {
                text: t.text.clone(),
                opts: text_opts.clone(),
                base_style: styles.text,
            },
            AssistantContent::Thinking(t) if t.redacted => MarkdownSegment {
                text: format!("[Redacted thinking: {}]", t.thinking),
                opts: thinking_opts.clone(),
                base_style: styles.thinking,
            },
            AssistantContent::Thinking(_) if hide_thinking => MarkdownSegment {
                text: "Thinking…".to_string(),
                opts: thinking_opts.clone(),
                base_style: styles.thinking,
            },
            AssistantContent::Thinking(t) => MarkdownSegment {
                text: format!("Thinking: {}", t.thinking),
                opts: thinking_opts.clone(),
                base_style: styles.thinking,
            },
            AssistantContent::ToolCall(_) => continue,
        };
        segments.push(segment);
    }
    MarkdownView::new(segments, Vec::new(), styles.markdown)
}

/// Build the [`MarkdownView`] for a compaction entry: a dim plain header above
/// the markdown-rendered summary.
///
/// The header stays a plain (non-markdown) leading row so its one-column inset
/// and token glyphs survive verbatim, carrying the `(<key> to expand)` hint
/// while collapsed. Folding rides the session-wide `tools_expanded` flag, the
/// same one tool bodies honor, so a summary expands and collapses together with
/// tool results under one keystroke. The summary renders as a markdown segment
/// only once expanded and non-empty.
fn build_compaction_markdown(
    c: &CompactionEntry,
    tools_expanded: bool,
    syntax_highlight: bool,
    styles: &TranscriptStyles,
) -> MarkdownView {
    let mut header = format!(" {}", compaction_header(c.tokens_before, c.tokens_after));
    if !tools_expanded && !c.summary.is_empty() {
        let key = EXPAND_KEY_LABEL.as_str();
        header.push_str(&format!(" ({key} to expand)"));
    }
    let leading = vec![TextSpan {
        text: header,
        style: styles.dim,
        ..TextSpan::default()
    }];
    let mut segments = Vec::new();
    if tools_expanded && !c.summary.is_empty() {
        segments.push(MarkdownSegment {
            text: c.summary.clone(),
            opts: RenderOpts {
                hyperlinks: styles.hyperlinks,
                default_emphasis: Emphasis::default(),
                syntax_highlight,
            },
            base_style: styles.text,
        });
    }
    MarkdownView::new(segments, leading, styles.markdown)
}

/// Header line for a compaction row: `Context compacted: 152k → 48k
/// tokens (freed 68%)`. `freed` is `0` when occupancy didn't drop (a
/// degenerate compaction), avoiding a misleading negative percentage.
#[allow(clippy::as_conversions)]
fn compaction_header(tokens_before: u64, tokens_after: u64) -> String {
    let freed = if tokens_before > tokens_after && tokens_before > 0 {
        ((tokens_before - tokens_after) as f64 / tokens_before as f64 * 100.0).round() as u64
    } else {
        0
    };
    format!(
        "Context compacted: {} → {} tokens (freed {freed}%)",
        format_tokens(tokens_before),
        format_tokens(tokens_after),
    )
}

/// A position inside one transcript entry, in the entry's own rendered-row
/// space at the current chat width: `line` is the wrapped-row index within the
/// entry and `col` is the display column (cell index) in that row, where
/// `col == width` means end-of-line. Ordering is document order: `EntryId` is
/// minted monotonically, so the derived tuple order (entry, line, col) sorts
/// positions the way they read top-to-bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SelPos {
    entry: EntryId,
    line: usize,
    col: usize,
}

/// A free-form transcript selection: an anchor and a caret, each an
/// entry-relative [`SelPos`]. Anchoring to `(entry, position)` rather than an
/// absolute viewport row means the highlight tracks its content across
/// scrolling and follow-tail (Spec E section 2). A zero-width selection
/// (`anchor == caret`) is a plain click with nothing highlighted.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Selection {
    anchor: SelPos,
    caret: SelPos,
}

/// The `(entry, line)` a screen row displays, produced by the per-frame walk
/// over realized entries. Used by the highlight to decide, per visible row,
/// which cells the selection covers.
#[derive(Clone, Copy)]
struct RowPos {
    entry: EntryId,
    line: usize,
}

/// The chat area: a follow-tail `ListView` over the active transcript,
/// wrapped in [`ScrollBars`] for the vertical scrollbar thumb (Spec E
/// section 1). The bar reserves the rightmost column and hides its
/// thumb while the transcript fits the viewport.
///
/// The bars stamp the list's widget identity, so content-area mouse
/// events hit-test to the list itself. This view is always an ancestor
/// in the hit list, so it observes those events in the capturing phase
/// (wheel-up disengages follow-tail, an active thumb drag is
/// intercepted). Events over the bar column hit-test to this view and
/// are forwarded from [`handle_event`](Widget::handle_event).
pub struct TranscriptView {
    chat: Rc<RefCell<ChatState>>,
    /// The chat list, shared with `bars`, which draws it and routes
    /// thumb drag-to-jump into it.
    list: Rc<RefCell<ListView>>,
    bars: ScrollBars<ListView>,
    /// Memoized per-entry surfaces, shared into the [`EntryBuilder`]. See
    /// [`EntryRenderCache`]. Owned here so it survives across frames and so a
    /// theme swap or a global-toggle change can clear it.
    cache: Rc<RefCell<EntryRenderCache>>,
    /// The transcript's styles, shared into the [`EntryBuilder`]. Kept here so
    /// the per-entry text layout ([`entry_rows`](Self::entry_rows)) builds
    /// entries with the same styles the visible list does. Replaced by
    /// [`set_styles`](Self::set_styles) on a theme swap.
    styles: Rc<TranscriptStyles>,
    /// Per-entry rendered rows, laid out on demand and cached. Select-to-copy
    /// extracts and highlights text out of these rather than a whole-transcript
    /// grid (Spec E section 2). See [`entry_rows`](Self::entry_rows).
    entry_text: EntryTextCache,
    /// `DrawContext` presentation state stashed from the last [`draw`], so the
    /// per-entry text layout builds under the same cell size and
    /// width-measurement method the visible render used and therefore wraps
    /// identically. The defaults are only a pre-first-draw fallback, and the
    /// layout is built on demand well after the first draw, so the stashed
    /// runtime values are what it actually uses.
    cell_size: Size,
    width_method: gwidth::Method,
    /// Last-seen session-wide render inputs. When any of these changes the
    /// whole cache is cleared, since they are not part of any per-entry
    /// fingerprint.
    last_globals: GlobalRenderInputs,
    /// While true, every draw pins the viewport to the bottom so a
    /// streaming turn stays in view. Wheel-up and thumb drags
    /// disengage, a scroll that lands back at the bottom re-engages
    /// (Spec E section 1). Suspended while the transcript is focused: the
    /// item cursor then owns the viewport, so [`draw`](Widget::draw) neither
    /// pins the bottom nor re-engages while focus mode is active.
    follow_tail: bool,
    /// Whether the transcript is in focus mode (Spec E section 1), shared by
    /// `Rc` with the [`EntryBuilder`] (so the focus border tracks the mode)
    /// and the keymap host context (so the copy chord is gated on it). This
    /// view is the single writer: [`enter_focus_mode`](Self::enter_focus_mode)
    /// and [`exit_focus_mode`](Self::exit_focus_mode), driven by focus in/out,
    /// set it.
    focused: Rc<std::cell::Cell<bool>>,
    /// Called from the Esc branch of transcript-focus mode to hand focus
    /// back to the editor. `None` until the host wires it in `Shell::new`.
    /// The resulting `FocusOut` clears the focus flag, exiting the mode.
    on_exit_focus: Option<Box<dyn FnMut(&mut EventContext)>>,
    /// The active free-form selection, if any (Spec E section 2). Set on a
    /// left-button press-drag over the content and kept highlighted after the
    /// release copies it, until the next plain click or Esc clears it.
    selection: Option<Selection>,
    /// Viewport size the last completed [`draw`](Widget::draw) laid out
    /// against. The mouse handlers run between draws with no `DrawContext`, so
    /// they read the geometry back from here to map widget-local coordinates
    /// into entry-relative selection positions. Zero before the first draw.
    last_view: Size,
}

/// The session-wide render inputs the transcript cache does not fingerprint
/// per entry. A change to any of them invalidates every cached surface, so
/// [`TranscriptView::draw`] clears the cache wholesale on a change. These
/// toggles are rare, so a full clear costs one all-miss frame.
#[derive(Clone, Copy, PartialEq, Eq)]
struct GlobalRenderInputs {
    active_view: AgentId,
    tools_expanded: bool,
    hide_thinking_block: bool,
    syntax_highlight: bool,
}

impl GlobalRenderInputs {
    fn read(chat: &ChatState) -> GlobalRenderInputs {
        GlobalRenderInputs {
            active_view: chat.active_view(),
            tools_expanded: chat.tools_expanded,
            hide_thinking_block: chat.hide_thinking_block,
            syntax_highlight: chat.syntax_highlight,
        }
    }
}

/// The graphemes of `row`, trailing blank cells trimmed. Interior blanks are
/// kept, so only the run of blanks at the end (default padding, or a selection
/// that ran to end-of-line) is dropped.
fn row_text(row: &[Cell]) -> String {
    let end = row
        .iter()
        .rposition(|cell| !cell.char.grapheme().trim().is_empty())
        .map_or(0, |i| i + 1);
    row[..end].iter().map(|cell| cell.char.grapheme()).collect()
}

/// Read the graphemes of the cell range `a..=b` out of `lines`, joining rows
/// with `\n`. Backs [`TranscriptView::extract_selection`], see it for the
/// contract.
fn extract_from_lines(lines: &[Vec<Cell>], a: (usize, usize), b: (usize, usize)) -> String {
    // A selection's anchor and caret may be in either order; normalize to
    // min..=max in (row, col) lexicographic order (tuples compare that way).
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    if start == end {
        return String::new();
    }
    let (start_row, start_col) = start;
    let (end_row, end_col) = end;

    let mut out = String::new();
    for row in start_row..=end_row {
        // Rows only increase, so an out-of-range row means nothing more to read.
        let Some(cells) = lines.get(row) else {
            break;
        };
        // The covered column span on this row: from `start_col` on the first
        // row, up to `end_col` on the last, whole otherwise. Clamped to the
        // row so an out-of-range column reads nothing rather than panicking.
        let from = if row == start_row { start_col } else { 0 };
        let to = if row == end_row { end_col } else { cells.len() };
        let from = from.min(cells.len());
        let to = to.min(cells.len()).max(from);
        if row != start_row {
            out.push('\n');
        }
        out.push_str(&row_text(&cells[from..to]));
    }
    out
}

impl TranscriptView {
    /// Build the view over `chat`. `focused` is the transcript-focus flag,
    /// shared with the keymap host context so the copy chord and the focus
    /// border read the same state (this view is its single writer).
    pub fn new(
        chat: Rc<RefCell<ChatState>>,
        theme: &Theme,
        focused: Rc<std::cell::Cell<bool>>,
    ) -> TranscriptView {
        let styles = Rc::new(TranscriptStyles::from_theme(theme));
        let cache = Rc::new(RefCell::new(EntryRenderCache::new()));
        let builder = EntryBuilder {
            chat: Rc::clone(&chat),
            styles: Rc::clone(&styles),
            cache: Rc::clone(&cache),
            focus_mode: Rc::clone(&focused),
            copy_label: Rc::new(copy_label_spans(&styles)),
        };
        let mut list = ListView::new(Source::Builder(Box::new(builder)));
        // `draw_cursor` stays off in every mode: the focused-message marker is
        // the border painted into the bubble padding (Spec E section 2), not a
        // cursor gutter. The list cursor still exists and moves under focus
        // navigation. `draw_cursor` only controls the gutter drawing.
        list.draw_cursor = false;
        let mut bars = ScrollBars::new(list);
        bars.draw_horizontal_scrollbar = false;
        apply_scrollbar_thumbs(&mut bars, &styles);
        let list = Rc::clone(&bars.view);
        let last_globals = GlobalRenderInputs::read(&chat.borrow());
        TranscriptView {
            chat,
            list,
            bars,
            cache,
            styles,
            entry_text: EntryTextCache::new(),
            // Match the shell's `DrawContext` defaults so a layout built before
            // the first draw wraps the same way the first visible frame will.
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
            last_globals,
            follow_tail: true,
            focused,
            on_exit_focus: None,
            selection: None,
            last_view: Size {
                width: 0,
                height: 0,
            },
        }
    }

    /// Re-engage follow-tail so the next draw pins the viewport to the
    /// bottom, and drop the render cache.
    ///
    /// Two callers use it. On a session rebuild the view's `chat` cell keeps
    /// its identity across the swap (the outer loop overwrites its contents
    /// in place), so the fresh session's transcript must open at the tail
    /// rather than wherever the previous session was scrolled. On an
    /// `active_view` switch each view opens at its bottom (Spec E section 1,
    /// per-view scroll). The draw path refreshes `item_count` before
    /// scrolling, so we needn't touch the list's scroll offset here.
    pub(crate) fn reset_to_tail(&mut self) {
        // Clear the cache: the reused `chat` cell now holds a different
        // session whose transcript restarts `EntryId` at 0, so its entries
        // collide with the previous session's cache keys `(AgentId, EntryId)`.
        // The draw-time global clear can't be relied on to catch this. A fresh
        // session's globals `(Main, tools_expanded=false, hide_thinking=false)`
        // usually match the outgoing session's, so no global change fires, and
        // a coincidental fingerprint+width match would then replay the old
        // session's surface. Length-proxy fingerprints make that coincidence
        // easy (two same-length prompts collide), so we drop every slot here.
        //
        // On a view switch the keys don't collide (different `AgentId`) and
        // the draw's global-input clear catches the change anyway, so the
        // clear here is redundant but harmless.
        self.cache.borrow_mut().clear();
        // The reused `chat` cell may now hold a different session whose entry
        // ids collide with the cached rows', the same hazard the render cache
        // clear above guards. Drop the per-entry text cache so select-to-copy
        // re-lays the new transcript's entries rather than reading the previous
        // session's rows.
        self.entry_text.clear();
        // The reused list holds a different session whose entries reuse indices,
        // so a geometry carried over from the previous session would missize the
        // new session's thumb. Drop it so the next draw rebuilds it.
        self.list.borrow_mut().reset_geometry();
        self.follow_tail = true;
        // A fresh session's entries are unrelated to the old selection's anchor
        // entry, so drop it rather than highlight stale content.
        self.selection = None;
    }

    /// Install the callback invoked when Esc leaves transcript-focus mode.
    /// The host wires it to move focus back to the editor (see `Shell::new`),
    /// whose `FocusOut` then clears the item cursor and exits the mode.
    pub(crate) fn set_on_exit_focus(&mut self, on_exit: Box<dyn FnMut(&mut EventContext)>) {
        self.on_exit_focus = Some(on_exit);
    }

    /// Entry count of the active view's transcript, or 0 when it has none.
    fn entry_count(&self) -> usize {
        let chat = self.chat.borrow();
        chat.transcript(chat.active_view())
            .map(|t| t.entries().len())
            .unwrap_or(0)
    }

    /// Whether the transcript is in focus mode. The mode lives on the shared
    /// [`focused`](Self::focused) flag, set by `FocusIn`/`FocusOut`.
    pub(crate) fn in_focus_mode(&self) -> bool {
        self.focused.get()
    }

    /// The entry indices of the active view's user messages, ascending
    /// (document order). Transcript-focus navigation steps between these,
    /// skipping assistant, tool, and other entries (Spec E section 1).
    fn user_message_indices(&self) -> Vec<usize> {
        let chat = self.chat.borrow();
        let Some(transcript) = chat.transcript(chat.active_view()) else {
            return Vec::new();
        };
        transcript
            .entries()
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches!(entry.kind, EntryKind::User(_)))
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Whether the active view has at least one user message. Guards the
    /// enter-focus chord: with none there is nothing to step to.
    pub(crate) fn has_user_message(&self) -> bool {
        !self.user_message_indices().is_empty()
    }

    /// Move the item cursor onto entry `idx` and bring it into view with a
    /// minimal scroll (`ensure_scroll`), so a step to a nearby user message
    /// keeps the surrounding replies on screen rather than jumping the
    /// viewport. `idx` must be a valid entry index. `item_count` is refreshed
    /// first so the move sees the current length even before the focused
    /// view's first draw.
    fn focus_item(&self, idx: usize) {
        let count = self.entry_count();
        let mut list = self.list.borrow_mut();
        list.item_count = Some(u32::try_from(count).expect("entry count fits u32"));
        list.cursor = u32::try_from(idx).expect("index fits u32");
        list.ensure_scroll();
    }

    /// Move the cursor onto the newest (last) user message. Used on entering
    /// focus mode and for the End / G jump. A no-op with no user message.
    fn focus_last_user_message(&self) {
        if let Some(&idx) = self.user_message_indices().last() {
            self.focus_item(idx);
        }
    }

    /// Move the cursor onto the oldest (first) user message. Home / g jump.
    /// A no-op with no user message.
    fn focus_first_user_message(&self) {
        if let Some(&idx) = self.user_message_indices().first() {
            self.focus_item(idx);
        }
    }

    /// Step to the next-older user message (toward index 0 from the current
    /// cursor). Clamps at the first, so a no-op once there is none older. It
    /// finds the nearest user message strictly above the cursor, which is
    /// defensive against a cursor that ever sits on a non-user entry.
    pub(crate) fn focus_prev_user_message(&self) {
        let cursor = usize::try_from(self.list.borrow().cursor).expect("cursor fits usize");
        if let Some(&idx) = self
            .user_message_indices()
            .iter()
            .rev()
            .find(|&&i| i < cursor)
        {
            self.focus_item(idx);
        }
    }

    /// Step to the next-newer user message (toward the end). Clamps at the
    /// last, so a no-op once there is none newer. It finds the nearest user
    /// message strictly below the cursor, defensive the same way
    /// [`focus_prev_user_message`](Self::focus_prev_user_message) is.
    fn focus_next_user_message(&self) {
        let cursor = usize::try_from(self.list.borrow().cursor).expect("cursor fits usize");
        if let Some(&idx) = self.user_message_indices().iter().find(|&&i| i > cursor) {
            self.focus_item(idx);
        }
    }

    /// The text of the focused user message, for the copy chord (Spec E
    /// section 2). `None` when not in focus mode or the cursor is not on a
    /// user message.
    ///
    /// Returns the message's own content (`joined_text`), which is the whole
    /// message the copy action promises, not the rendered cells (that would
    /// carry the border glyphs and padding).
    pub(crate) fn focused_message_text(&self) -> Option<String> {
        if !self.in_focus_mode() {
            return None;
        }
        let idx = usize::try_from(self.list.borrow().cursor).ok()?;
        let chat = self.chat.borrow();
        let entry = chat.transcript(chat.active_view())?.entries().get(idx)?;
        match &entry.kind {
            EntryKind::User(user) => Some(user.joined_text()),
            _ => None,
        }
    }

    /// Enter transcript-focus mode (Spec E section 1): set the shared focus
    /// flag, suspend follow-tail so the cursor is not fought by auto-scroll,
    /// and land the cursor on the newest user message. Driven by
    /// `Event::FocusIn`, so the mode is exactly "the transcript is the focused
    /// widget".
    fn enter_focus_mode(&mut self, ctx: &mut EventContext) {
        // The focus flag drives the per-entry border (via the shared cell the
        // `EntryBuilder` reads) that marks the focused message. `draw_cursor`
        // stays off (Spec E section 2).
        self.focused.set(true);
        self.follow_tail = false;
        self.focus_last_user_message();
        ctx.redraw = true;
    }

    /// Leave transcript-focus mode: clear the shared focus flag. Driven by
    /// `Event::FocusOut`, so it also fires when an opening overlay steals focus
    /// (the overlay's `request_focus` sends this a `FocusOut`).
    fn exit_focus_mode(&self, ctx: &mut EventContext) {
        self.focused.set(false);
        ctx.redraw = true;
    }

    /// Handle a key press while the transcript is focused, stepping the item
    /// cursor between user messages (Spec E section 1). A no-op when not
    /// focused (the key then falls through unconsumed).
    ///
    /// Tab is not handled here: the global capture-phase chord owns it and
    /// dispatches to [`focus_prev_user_message`](Self::focus_prev_user_message).
    /// PageUp/PageDown and Home/End are likewise globally-bound capture-phase
    /// chords, consumed ahead of the focused transcript and dispatched to the
    /// mode-aware [`scroll_to_top`](Self::scroll_to_top) /
    /// [`scroll_to_bottom`](Self::scroll_to_bottom).
    fn handle_focus_key(&mut self, ctx: &mut EventContext, key: &Key) {
        if !self.in_focus_mode() {
            return;
        }
        let empty = Modifiers::empty();
        let ctrl = Modifiers::CTRL;
        if key.matches(Key::UP, empty)
            || key.matches(u32::from('k'), empty)
            || key.matches(u32::from('p'), ctrl)
        {
            self.focus_prev_user_message();
            ctx.consume_and_redraw();
        } else if key.matches(Key::DOWN, empty)
            || key.matches(u32::from('j'), empty)
            || key.matches(u32::from('n'), ctrl)
            || key.matches(Key::TAB, Modifiers::SHIFT)
        {
            self.focus_next_user_message();
            ctx.consume_and_redraw();
        } else if key.matches(u32::from('g'), empty) {
            self.focus_first_user_message();
            ctx.consume_and_redraw();
        } else if key.matches(u32::from('g'), Modifiers::SHIFT) {
            self.focus_last_user_message();
            ctx.consume_and_redraw();
        } else if key.matches(Key::ESCAPE, empty) {
            if let Some(on_exit) = self.on_exit_focus.as_mut() {
                on_exit(ctx);
            }
            ctx.consume_and_redraw();
        }
    }

    /// Scroll the transcript up by one viewport page (Spec E section 1).
    ///
    /// A manual scroll up means the reader wants history, so follow-tail
    /// disengages and new content stops yanking the viewport to the bottom.
    /// (Paging up on a transcript that fits the viewport can't move it, so
    /// the draw re-engages follow-tail right away, leaving it pinned.)
    pub(crate) fn page_up(&mut self) {
        self.follow_tail = false;
        let lines = self.page_lines();
        self.list.borrow_mut().scroll_lines(-lines);
    }

    /// Scroll the transcript down by one viewport page (Spec E section 1).
    ///
    /// Unlike [`page_up`](Self::page_up) this takes `&self`: it never touches
    /// `follow_tail` directly. If the scroll lands back at the bottom the next
    /// draw re-engages follow-tail (see [`draw`](Widget::draw)), so paging
    /// down to the end resumes following streamed content.
    pub(crate) fn page_down(&self) {
        let lines = self.page_lines();
        self.list.borrow_mut().scroll_lines(lines);
    }

    /// Scroll the transcript to the top (Spec E section 1, Home), mode-aware.
    pub(crate) fn scroll_to_top(&mut self) {
        if self.in_focus_mode() {
            // Focus mode: move the item cursor onto the first user message,
            // matching the `g` jump.
            self.focus_first_user_message();
            return;
        }
        // Reaching the top means the reader left the tail, so follow-tail
        // disengages. `jump_to_item(0)` pins the scroll window to item 0 at
        // offset 0, the very first line, rather than only moving the hidden
        // cursor.
        self.follow_tail = false;
        self.list.borrow_mut().jump_to_item(0);
    }

    /// Scroll the transcript to the bottom (Spec E section 1, End), mode-aware.
    pub(crate) fn scroll_to_bottom(&mut self) {
        if self.in_focus_mode() {
            // Focus mode: move the item cursor onto the last user message,
            // matching the `G` jump.
            self.focus_last_user_message();
            return;
        }
        // Re-engaging follow-tail is the whole gesture: the next draw runs the
        // inner list's `scroll_to_bottom` and the transcript resumes following
        // the tail (see [`draw`](Widget::draw)).
        self.follow_tail = true;
    }

    /// One page of scroll in lines: the last-drawn viewport height minus a
    /// small overlap so a page turn keeps a couple of rows of context. Falls
    /// back to [`DEFAULT_PAGE_LINES`] before the first draw has measured the
    /// viewport.
    fn page_lines(&self) -> i32 {
        match self.list.borrow().viewport_height() {
            Some(h) if h > PAGE_OVERLAP => i32::from(h - PAGE_OVERLAP),
            // A viewport too short to overlap still pages by at least one row.
            Some(h) if h > 0 => i32::from(h),
            _ => DEFAULT_PAGE_LINES,
        }
    }

    /// Rebuild the transcript's styles from a fresh palette, for a
    /// runtime theme swap. Replaces the row builder (so the per-entry
    /// widgets, which are rebuilt every frame, pick up the new colors)
    /// and re-applies the scrollbar thumb tints. Scroll position is
    /// left untouched, so a reload doesn't jump the viewport.
    pub(crate) fn set_styles(&mut self, styles: Rc<TranscriptStyles>) {
        // A theme swap re-tints every entry, so every cached surface is stale.
        self.cache.borrow_mut().clear();
        // The per-entry text cache holds re-tinted cells too, so drop it.
        self.entry_text.clear();
        let builder = EntryBuilder {
            chat: Rc::clone(&self.chat),
            styles: Rc::clone(&styles),
            cache: Rc::clone(&self.cache),
            focus_mode: Rc::clone(&self.focused),
            copy_label: Rc::new(copy_label_spans(&styles)),
        };
        self.list.borrow_mut().children = Source::Builder(Box::new(builder));
        apply_scrollbar_thumbs(&mut self.bars, &styles);
        self.styles = styles;
    }
}

impl TranscriptView {
    /// The active view's entry `id` laid out at content width `width` into its
    /// rendered rows (cells), cached per entry and reused while the entry's
    /// fingerprint and width hold. An unknown id yields no rows.
    ///
    /// Wraps identically to the visible render because it builds the entry
    /// through the same [`build_entry_widget`] under the cell size and width
    /// method stashed from the last draw.
    fn entry_rows(&mut self, id: EntryId, width: u16) -> Rc<Vec<Vec<Cell>>> {
        // Resolve the live entry's fingerprint under a short borrow that we
        // drop before the hit check. The miss path below re-borrows and holds
        // the borrow across `widget.draw`, which is safe because the entry
        // widget captures no `chat` handle (the same rationale as the visible
        // `CachingEntry`).
        let fingerprint = {
            let chat = self.chat.borrow();
            let agent = chat.active_view();
            match chat.transcript(agent).and_then(|t| t.get(id)) {
                Some(entry) => entry_fingerprint(entry, &chat),
                None => return Rc::new(Vec::new()),
            }
        };
        if let Some(rows) = self.entry_text.get(id, fingerprint, width) {
            return rows;
        }
        // MISS: lay the entry out under the same per-entry constraints the
        // visible `ListView` draws each child under (min/max width `width`,
        // height unbounded so the entry yields all its rows) and the stashed
        // presentation state, so the rows wrap the way the render does.
        let ctx = DrawContext {
            min: Size { width, height: 0 },
            max: MaxSize {
                width: Some(width),
                height: None,
            },
            cell_size: self.cell_size,
            width_method: self.width_method,
        };
        let rows = {
            let chat = self.chat.borrow();
            let agent = chat.active_view();
            let Some(entry) = chat.transcript(agent).and_then(|t| t.get(id)) else {
                return Rc::new(Vec::new());
            };
            let mut widget =
                build_entry_widget(entry, &chat, &self.styles, false, None).into_boxed();
            let surface = widget.draw(&ctx);
            let mut rows = surface_rows(&surface);
            for row in &mut rows {
                // The list composites entries into a `width`-wide surface, so a
                // short row is padded and an over-wide one clipped there. Match
                // that so a coordinate lines up with the visible grid.
                row.resize(usize::from(width), Cell::default());
            }
            rows
        };
        let rows = Rc::new(rows);
        self.entry_text
            .insert(id, fingerprint, width, Rc::clone(&rows));
        rows
    }

    /// Rendered-row count of entry `id` at `width`, at least 1.
    ///
    /// The floor guards a per-entry walk against a zero-height entry stalling
    /// it. Entries always render at least a trailing blank row, so it is
    /// defensive.
    fn entry_height(&mut self, id: EntryId, width: u16) -> usize {
        self.entry_rows(id, width).len().max(1)
    }

    /// The `EntryId` at index `idx` of the active view's transcript, if any.
    /// Takes a short `self.chat` borrow, so a caller must not hold another one
    /// across the call (in particular, do not nest it with `entry_height`).
    fn entry_id_at(&self, idx: usize) -> Option<EntryId> {
        let chat = self.chat.borrow();
        chat.transcript(chat.active_view())?
            .entries()
            .get(idx)
            .map(|e| e.id)
    }

    /// The text covered by the selection at content width `width`, as
    /// `[start, end]` in [`SelPos`] order (either endpoint may come first).
    ///
    /// Walks the spanned entries, laying each out on demand, takes the covered
    /// row/col range out of each via [`extract_from_lines`], and joins entries
    /// with `\n`. Bounded by the selection length, not the transcript length.
    fn extract_selection(&mut self, width: u16, a: SelPos, b: SelPos) -> String {
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        if start == end {
            return String::new();
        }
        // Snapshot the spanned entry ids under a short borrow. `EntryId` order
        // is document order, so the span is a contiguous id range.
        let ids: Vec<EntryId> = {
            let chat = self.chat.borrow();
            let agent = chat.active_view();
            match chat.transcript(agent) {
                Some(t) => t
                    .entries()
                    .iter()
                    .filter(|e| e.id >= start.entry && e.id <= end.entry)
                    .map(|e| e.id)
                    .collect(),
                None => Vec::new(),
            }
        };
        let mut parts: Vec<String> = Vec::with_capacity(ids.len());
        for id in ids {
            let rows = self.entry_rows(id, width);
            // The start entry is read from the anchor position, the end entry
            // up to the caret position, and whole entries in between.
            let from_line = if id == start.entry { start.line } else { 0 };
            let from_col = if id == start.entry { start.col } else { 0 };
            let to_line = if id == end.entry {
                end.line
            } else {
                rows.len().saturating_sub(1)
            };
            let to_col = if id == end.entry {
                end.col
            } else {
                usize::from(width)
            };
            parts.push(extract_from_lines(
                &rows,
                (from_line, from_col),
                (to_line, to_col),
            ));
        }
        // An entry boundary is a newline, matching how the concatenated rows of
        // adjacent entries would join.
        parts.join("\n")
    }
}

impl TranscriptView {
    /// Mouse observation shared by both dispatch phases: an active
    /// thumb drag is handed to the bars (and disengages follow-tail
    /// when it moves the viewport), and any manual wheel-up means the
    /// user wants to read history, so new content must stop yanking
    /// the viewport to the bottom.
    fn observe_mouse(&mut self, ctx: &mut EventContext, event: &Event, m: &mouse::Mouse) {
        self.bars.capture_event(ctx, event);
        if ctx.consume_event {
            if m.kind == mouse::Type::Drag {
                self.follow_tail = false;
            }
            return;
        }
        if m.button == mouse::Button::WheelUp {
            self.follow_tail = false;
        }
    }

    /// The content width the list lays entries out at: the last viewport width
    /// minus the reserved scrollbar column. Per-entry layout must be queried
    /// at this width to align with the render.
    ///
    /// There is no cursor gutter to subtract: the focus marker is a border
    /// inside the bubble padding, so entries are laid out at the same width in
    /// every mode (Spec E section 2).
    fn content_width(&self) -> u16 {
        self.last_view.width.saturating_sub(1)
    }

    /// Map a widget-local mouse position to an entry-relative [`SelPos`].
    ///
    /// `m_row`/`m_col` are TranscriptView-local (row 0 = the chat slot's top).
    /// We clamp `m_row` into the viewport, add the top entry's hidden-line
    /// offset to get the line from the top entry's start, then walk realized
    /// entries top to bottom (each at most a viewport of rows) until that line
    /// lands inside one. `col` is `m_col` clamped into `[0, width]` (the
    /// scrollbar owns the last column, the far edge is end-of-line).
    /// Returns `None` on an empty transcript, where there is nothing to select.
    fn point_to_sel(&mut self, m_row: i16, m_col: i16) -> Option<SelPos> {
        let width = self.content_width();
        let (top_idx, off) = {
            let list = self.list.borrow();
            (
                usize::try_from(list.scroll_top()).unwrap_or(0),
                list.scroll_offset(),
            )
        };
        let height = i32::from(self.last_view.height);
        let local_row = i32::from(m_row).clamp(0, (height - 1).max(0));
        // The line from the top entry's first line: the hidden-above offset
        // plus how far down the viewport the click landed.
        let target = usize::try_from(off)
            .unwrap_or(0)
            .saturating_add(usize::try_from(local_row).unwrap_or(0));
        let content_col = i32::from(m_col).clamp(0, i32::from(width));
        let col = usize::try_from(content_col).unwrap_or(0);

        // Walk entries from the top, subtracting each entry's height until the
        // target line lands inside one.
        let mut remaining = target;
        let mut idx = top_idx;
        while let Some(id) = self.entry_id_at(idx) {
            let h = self.entry_height(id, width);
            if remaining < h {
                return Some(SelPos {
                    entry: id,
                    line: remaining,
                    col,
                });
            }
            remaining -= h;
            idx += 1;
        }
        // The target sits past the last entry (a drag past the bottom on a
        // short transcript), so clamp to the last entry's last line. An empty
        // transcript falls out as `None`.
        let count = self.entry_count();
        let last = self.entry_id_at(count.checked_sub(1)?)?;
        let line = self.entry_height(last, width).saturating_sub(1);
        Some(SelPos {
            entry: last,
            line,
            col,
        })
    }

    /// Drive the free-form selection from a left-button mouse event (Spec E
    /// section 2). Called only after the bars declined the event, so a
    /// scrollbar-thumb drag scrolls rather than selects.
    fn handle_selection_mouse(&mut self, ctx: &mut EventContext, m: &mouse::Mouse) {
        match m.kind {
            mouse::Type::Press => {
                // A press anchors a fresh (zero-width) selection and stops the
                // viewport chasing the tail so the anchor stays put.
                let Some(pos) = self.point_to_sel(m.row, m.col) else {
                    return;
                };
                self.selection = Some(Selection {
                    anchor: pos,
                    caret: pos,
                });
                self.follow_tail = false;
                ctx.redraw = true;
            }
            mouse::Type::Drag => {
                // Dragging past the top or bottom edge auto-scrolls by the
                // overshoot so a selection can span more than one screen. The
                // revealed rows extend the selection on subsequent frames.
                let height = i16::try_from(self.last_view.height).unwrap_or(i16::MAX);
                if m.row < 0 {
                    self.list.borrow_mut().scroll_lines(i32::from(m.row));
                } else if m.row >= height {
                    self.list
                        .borrow_mut()
                        .scroll_lines(i32::from(m.row - height + 1));
                }
                let Some(caret) = self.point_to_sel(m.row, m.col) else {
                    ctx.redraw = true;
                    return;
                };
                match self.selection.as_mut() {
                    Some(sel) => sel.caret = caret,
                    // A drag with no prior press is not expected, but start a
                    // selection at the caret rather than drop the interaction.
                    None => {
                        self.selection = Some(Selection {
                            anchor: caret,
                            caret,
                        })
                    }
                }
                ctx.redraw = true;
            }
            mouse::Type::Release => {
                if let Some(sel) = self.selection {
                    if sel.anchor == sel.caret {
                        // A plain click (no drag) clears the selection.
                        self.selection = None;
                    } else {
                        // Select-to-copy: a real range copies to the clipboard
                        // via OSC 52 and stays highlighted until the next click
                        // or Esc.
                        let width = self.content_width();
                        let text = self.extract_selection(width, sel.anchor, sel.caret);
                        ctx.copy_to_clipboard(text);
                    }
                    ctx.redraw = true;
                }
            }
            mouse::Type::Motion => {}
        }
    }

    /// For each of the `height` visible screen rows, the `(entry, line)` it
    /// displays, or `None` for a row past the end of content.
    ///
    /// Walks realized entries once from the top, so it is O(viewport) rather
    /// than O(viewport * entries). The top entry's hidden-above offset seeds
    /// the starting line within that entry.
    fn visible_row_positions(&mut self, height: usize) -> Vec<Option<RowPos>> {
        let width = self.content_width();
        let (top_idx, off) = {
            let list = self.list.borrow();
            (
                usize::try_from(list.scroll_top()).unwrap_or(0),
                usize::try_from(list.scroll_offset()).unwrap_or(0),
            )
        };
        let mut out: Vec<Option<RowPos>> = Vec::with_capacity(height);
        let mut idx = top_idx;
        // The first visible row shows the top entry's line `off`, the rows
        // before it being hidden above the top edge.
        let mut line = off;
        // The current entry and its height, refreshed as the walk crosses
        // entries. `entry_id_at` and `entry_height` each take a short
        // `self.chat` borrow, so we call them one after another, never nested.
        let mut current = match self.entry_id_at(idx) {
            Some(id) => Some((id, self.entry_height(id, width))),
            None => None,
        };
        for _ in 0..height {
            // Advance past any entries the running line has walked off the end
            // of. Every entry is at least one row tall, so this consumes at
            // most one entry per screen row and the walk stays O(viewport).
            while let Some((_, h)) = current {
                if line < h {
                    break;
                }
                idx += 1;
                line = 0;
                current = match self.entry_id_at(idx) {
                    Some(id) => Some((id, self.entry_height(id, width))),
                    None => None,
                };
            }
            match current {
                Some((id, _)) => {
                    out.push(Some(RowPos { entry: id, line }));
                    line += 1;
                }
                None => out.push(None),
            }
        }
        out
    }

    /// Paint the active selection's highlight over the composed frame.
    ///
    /// Runs after the bars surface is composed into `surface`. We read the
    /// composited cells back out and push a top-most overlay that copies every
    /// cell verbatim except the selected ones, whose background is set to the
    /// selection color. The cached entry rows are never touched: the highlight
    /// lives only on this per-frame copy, so it is never baked in and tracks
    /// the content as the viewport scrolls.
    fn paint_selection(&mut self, surface: &mut Surface, sel: Selection) {
        let (min, max) = if sel.anchor <= sel.caret {
            (sel.anchor, sel.caret)
        } else {
            (sel.caret, sel.anchor)
        };
        let width = usize::from(self.content_width());
        let selection_bg = self.styles.selection_bg;

        let grid = surface_rows(surface);
        let height = grid.len();
        if height == 0 {
            return;
        }
        // The `(entry, line)` shown at each screen row. Selection endpoints
        // compare against these as `(entry, line)` tuples, in document order.
        let row_positions = self.visible_row_positions(height);
        let lo = (min.entry, min.line);
        let hi = (max.entry, max.line);
        // Nothing to paint when no visible row falls in the selected range.
        let any_covered = row_positions
            .iter()
            .flatten()
            .any(|rp| (rp.entry, rp.line) >= lo && (rp.entry, rp.line) <= hi);
        if !any_covered {
            return;
        }

        let mut overlay = Surface::with_size(surface.size);
        for (r, row) in grid.iter().enumerate() {
            // The covered content-column span on this screen row: from the
            // anchor col on the first covered row, up to the caret col on the
            // last, whole rows between.
            let (from, to) = match row_positions.get(r).copied().flatten() {
                Some(rp) => {
                    let here = (rp.entry, rp.line);
                    if here < lo || here > hi {
                        (0, 0)
                    } else if here == lo && here == hi {
                        (min.col, max.col)
                    } else if here == lo {
                        (min.col, width)
                    } else if here == hi {
                        (0, max.col)
                    } else {
                        (0, width)
                    }
                }
                None => (0, 0),
            };
            let (from, to) = (from.min(width), to.min(width));
            for (c, cell) in row.iter().enumerate() {
                let mut cell = cell.clone();
                if to > from {
                    // Screen col `c` is the content col directly (no gutter).
                    // The scrollbar column sits past `width` and so is never
                    // hit.
                    if c >= from && c < to {
                        cell.style.bg = selection_bg;
                        // A highlighted cell is painted, not blank, so
                        // clear `default`. Otherwise the diff's default
                        // fast-path mistakes a highlighted blank cell (a
                        // trailing space or gap inside the selection) for
                        // an untouched one and skips repainting it, which
                        // leaves the highlight torn as the drag redraws.
                        cell.default = false;
                    }
                }
                let (Ok(col), Ok(row_u16)) = (u16::try_from(c), u16::try_from(r)) else {
                    continue;
                };
                overlay.write_cell(col, row_u16, cell);
            }
        }
        surface.children.push(SubSurface {
            origin: RelativePoint { row: 0, col: 0 },
            surface: overlay,
            z_index: 1,
        });
    }
}

/// Set the scrollbar's muted-thumb tints from the transcript palette:
/// a dim thumb that brightens on hover and to the text color while
/// dragged.
fn apply_scrollbar_thumbs(bars: &mut ScrollBars<ListView>, styles: &TranscriptStyles) {
    let thumb = |grapheme: &str, style: Style| Cell {
        char: Character::new(grapheme, 1),
        style,
        ..Cell::default()
    };
    bars.vertical_scrollbar_thumb = thumb("\u{2590}", styles.scrollbar_thumb);
    bars.vertical_scrollbar_hover_thumb = thumb("\u{2588}", styles.scrollbar_thumb);
    bars.vertical_scrollbar_drag_thumb = thumb("\u{2588}", styles.text);
}

impl Widget for TranscriptView {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // Stash the presentation state the per-entry text layout reuses, so
        // `entry_rows` wraps under the same cell size and width method the
        // visible render just used. A measuring pass carries the same values,
        // so we stash before the unbounded-height early return below.
        self.cell_size = ctx.cell_size;
        self.width_method = ctx.width_method;
        // A flex parent's measuring pass draws children under an
        // unbounded height. The transcript has no inherent height (its
        // flex share decides it), so report zero and skip the list
        // layout entirely, it needs a bounded viewport.
        if ctx.max.height.is_none() {
            return Surface::with_size(vaxis::vxfw::Size {
                width: ctx.max.width.unwrap_or(0),
                height: 0,
            });
        }
        // Global render inputs are not fingerprinted per entry, so a change to
        // any of them invalidates every cached surface. Compare against last
        // frame and clear wholesale on a change. These toggles are rare, so a
        // full clear costs one all-miss frame. A width change is handled
        // per-slot (width mismatch = miss), so it needs no global clear.
        let globals = GlobalRenderInputs::read(&self.chat.borrow());
        if globals != self.last_globals {
            self.cache.borrow_mut().clear();
            // The per-entry text cache keys on the same fingerprint, which also
            // omits these globals, so it must drop with the render cache. A
            // live selection reads the cached rows, so a stale hit here would
            // highlight and copy the pre-toggle wrapping.
            self.entry_text.clear();
            self.last_globals = globals;
        }
        let count = self.entry_count();
        // Focus mode hands the viewport to the item cursor, so follow-tail
        // must neither pin the bottom nor re-engage while it is active, or the
        // auto-scroll fights the cursor navigation (Spec E section 1).
        let focus_mode = self.in_focus_mode();
        {
            let mut list = self.list.borrow_mut();
            // The builder has no inherent end-of-list knowledge worth
            // walking for, so refresh the exact count every draw. It also
            // makes `scroll_to_bottom` cheap (no builder walk).
            list.item_count = Some(u32::try_from(count).expect("entry count fits u32"));
            if self.follow_tail && !focus_mode {
                list.scroll_to_bottom();
            }
        }
        // The bars draw the list one column narrower and add the thumb
        // when the reconciled scroll says the transcript overflows.
        let bars_surface = self.bars.draw(ctx);
        // The draw reconciled any pending wheel scroll, so "we are at
        // the bottom" is now accurate. Landing there re-engages
        // follow-tail (except in focus mode, where the cursor owns the
        // viewport).
        if !focus_mode && self.list.borrow().is_at_bottom() {
            self.follow_tail = true;
        }
        // Wrap the bars in an opaque full-size surface: the list draws
        // no background of its own (draw_cursor off), and without one
        // stale cells from the previous frame would survive a scroll.
        let mut surface = Surface::with_size(ctx.max.size());
        surface.children.push(SubSurface {
            origin: RelativePoint { col: 0, row: 0 },
            surface: bars_surface,
            z_index: 0,
        });
        // Record the viewport this draw laid out against, so the between-draw
        // mouse handlers can map screen coordinates into entry-relative
        // selection positions.
        self.last_view = ctx.max.size();
        // Paint the selection highlight over the composed frame (Spec E
        // section 2). A zero-width selection (a plain click) shows nothing.
        if let Some(sel) = self.selection {
            if sel.anchor != sel.caret {
                self.paint_selection(&mut surface, sel);
            }
        }
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        // Content-area mouse events target the inner list, so they
        // pass through here on the way down.
        if let Event::Mouse(m) = event {
            self.observe_mouse(ctx, event, m);
        }
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            Event::Mouse(m) => {
                self.observe_mouse(ctx, event, m);
                if ctx.consume_event {
                    return;
                }
                // Thumb hover and press-to-drag live in the bars'
                // bubbling-phase handler.
                self.bars.handle_event(ctx, event);
                if ctx.consume_event {
                    return;
                }
                // Free-form selection over the content area (Spec E section 2).
                // Runs only for the left button and only now that the bars
                // declined the event, so a scrollbar-thumb drag scrolls rather
                // than selects. The inner list ignores the left button, so we
                // needn't forward it on.
                if m.button == mouse::Button::Left {
                    self.handle_selection_mouse(ctx, m);
                    return;
                }
                if m.button == mouse::Button::WheelUp {
                    self.follow_tail = false;
                }
                self.list.borrow_mut().handle_event(ctx, event);
            }
            // The bars cancel an in-flight drag when the mouse leaves.
            Event::MouseLeave => self.bars.handle_event(ctx, event),
            // Focus in/out drive transcript-focus mode: the transcript is "in
            // focus mode" exactly when it is the focused widget (Spec E
            // section 1). FocusOut also fires when an opening overlay steals
            // focus, which cleanly exits the mode.
            Event::FocusIn => self.enter_focus_mode(ctx),
            Event::FocusOut => self.exit_focus_mode(ctx),
            Event::KeyPress(key) => {
                // Esc clears a live selection first (Spec E section 2), before
                // the focus-mode Esc would leave the mode, so one Esc drops the
                // highlight and a second exits focus.
                if key.matches(Key::ESCAPE, Modifiers::empty()) && self.selection.is_some() {
                    self.selection = None;
                    return ctx.consume_and_redraw();
                }
                // Item navigation, live only while focused (see
                // `handle_focus_key`).
                self.handle_focus_key(ctx, key);
            }
            _ => {}
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_agent::events::AgentEvent;
    use aj_agent::message::AgentMessage;
    use aj_agent::tool::TaskKind;
    use aj_app::chat::{
        AssistantEntry, CompactionEntry, EntryId, NoticeEntry, Transcript, UserEntry, reduce,
    };
    use aj_app::session::AgentLifecycle;
    use aj_models::streaming::AssistantMessageEvent;
    use aj_models::types::{
        AssistantContent, AssistantMessage, Message, StopReason, TextContent, ThinkingContent,
        UserContent,
    };
    use vaxis::vxfw::{MaxSize, Size};

    use super::*;

    fn styles() -> TranscriptStyles {
        TranscriptStyles::from_theme(&Theme::bundled_dark_with_mode(
            aj_app::theme::ColorMode::Truecolor,
        ))
    }

    fn draw_ctx(width: u16, height: u16) -> DrawContext {
        DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(width),
                height: Some(height),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: vaxis::gwidth::Method::Unicode,
        }
    }

    fn assistant_message(content: Vec<AssistantContent>) -> AssistantMessage {
        AssistantMessage {
            content,
            api: "scripted".into(),
            provider: "scripted".into(),
            model: "scripted".into(),
            response_id: None,
            usage: Default::default(),
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        }
    }

    /// Wrap `kind` in a one-entry transcript, since entries are only
    /// mintable through `Transcript::append`.
    fn transcript_with(kind: EntryKind) -> Transcript {
        let mut t = Transcript::default();
        let _: EntryId = t.append(kind);
        t
    }

    fn joined(spans: &[TextSpan]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// Draw the assistant entry's `MarkdownView` at `width` and return its
    /// composited rows.
    fn assistant_markdown_rows(t: &Transcript, hide_thinking: bool, width: u16) -> Vec<String> {
        let EntryKind::Assistant(a) = &t.entries()[0].kind else {
            panic!("expected an assistant entry");
        };
        let mut view = build_assistant_markdown(a, hide_thinking, false, &styles());
        let surface = view.draw(&crate::test_support::draw_ctx(width, None));
        crate::test_support::rows(&surface)
    }

    /// Draw the compaction entry's `MarkdownView` at `width` and return its
    /// composited rows plus the header's (first visible cell's) style.
    fn compaction_view_rows(t: &Transcript, expanded: bool, width: u16) -> (Vec<String>, Style) {
        let EntryKind::Compaction(c) = &t.entries()[0].kind else {
            panic!("expected a compaction entry");
        };
        let mut view = build_compaction_markdown(c, expanded, false, &styles());
        let surface = view.draw(&crate::test_support::draw_ctx(width, None));
        let rows = crate::test_support::rows(&surface);
        let grid = crate::test_support::flatten(&surface);
        let header_style = grid[0]
            .iter()
            .find(|cell| !cell.char.grapheme().trim().is_empty())
            .map(|cell| cell.style)
            .unwrap_or_default();
        (rows, header_style)
    }

    #[test]
    fn user_entry_spans_are_empty_the_bubble_renders_it() {
        // User entries render through `build_user_bubble`, so the
        // span path only carries the spacer.
        let t = transcript_with(EntryKind::User(UserEntry {
            content: vec![UserContent::text("hello")],
            collapsible: false,
        }));
        let spans = entry_spans(&t.entries()[0], &styles());
        assert_eq!(joined(&spans), "\n\n");
    }

    /// A long task notification with a recognisable first line and a
    /// tail marker well past [`USER_COLLAPSED_LINES`].
    fn notification() -> UserEntry {
        let mut lines = vec![
            "<task-notification>".to_string(),
            "Background task #1 finished: sleep - exit code 0".to_string(),
        ];
        for i in 1..30 {
            lines.push(format!("tick {i}"));
        }
        lines.push("SECRET_TAIL_MARKER".to_string());
        lines.push("</task-notification>".to_string());
        UserEntry {
            content: vec![UserContent::text(lines.join("\n"))],
            collapsible: true,
        }
    }

    fn bubble_rows(user: &UserEntry, expanded: bool, width: u16) -> Vec<String> {
        let mut bubble = build_user_bubble(user, expanded, &styles(), None);
        let surface = bubble.draw(&crate::test_support::draw_ctx(width, None));
        crate::test_support::rows(&surface)
    }

    #[test]
    fn user_bubble_paints_the_tint_and_drops_the_prefix() {
        let user = UserEntry {
            content: vec![UserContent::text("hello world")],
            collapsible: false,
        };
        let s = styles();
        let mut bubble = build_user_bubble(&user, false, &s, None);
        let surface = bubble.draw(&crate::test_support::draw_ctx(40, None));
        let r = crate::test_support::rows(&surface);
        // One padding row above and below the content, then the
        // untinted spacer row. No `> ` quote prefix: the tint is the
        // whole cue.
        assert_eq!(r.len(), 4, "{r:?}");
        assert_eq!(r[0], "");
        assert_eq!(r[1], " hello world");
        assert_eq!(r[2], "");
        assert_eq!(r[3], "");
        let grid = crate::test_support::flatten(&surface);
        for row in grid.iter().take(3) {
            for cell in row {
                assert_eq!(cell.style.bg, s.user_message_bg);
            }
        }
        assert!(grid[3].iter().all(|c| c.style.bg == Color::Default));
    }

    #[test]
    fn collapsible_notification_folds_to_ten_lines_with_italic_hint() {
        let user = notification();
        let r = bubble_rows(&user, false, 80);
        let body = r.join("\n");
        assert!(body.contains("Background task #1 finished"), "{r:?}");
        assert!(!body.contains("SECRET_TAIL_MARKER"), "{r:?}");
        // 10 source lines + the hint row + 2 pads + spacer.
        assert!(
            body.contains("more lines, Alt+O to expand)"),
            "hint present: {r:?}",
        );
        assert_eq!(r.len(), 10 + 1 + 2 + 1, "{r:?}");
        // The hint row is italic.
        let s = styles();
        let mut bubble = build_user_bubble(&user, false, &s, None);
        let surface = bubble.draw(&crate::test_support::draw_ctx(80, None));
        let grid = crate::test_support::flatten(&surface);
        let hint_row = &grid[11];
        assert!(
            hint_row
                .iter()
                .filter(|c| !c.char.grapheme().trim().is_empty())
                .all(|c| c.style.italic),
            "hint cells are italic",
        );
    }

    #[test]
    fn expanded_notification_shows_the_full_body() {
        let user = notification();
        let r = bubble_rows(&user, true, 80);
        let body = r.join("\n");
        assert!(body.contains("SECRET_TAIL_MARKER"), "{r:?}");
        assert!(!body.contains("to expand"), "{r:?}");
    }

    /// The focus border paints into the bubble's existing padding, so a
    /// focused bubble has the exact same surface size as an unfocused one.
    /// Gaining or losing focus therefore never reflows the transcript (Spec E
    /// section 2).
    #[test]
    fn focus_border_reuses_the_padding_and_keeps_the_bubble_size() {
        let user = UserEntry {
            content: vec![UserContent::text("ciao?")],
            collapsible: false,
        };
        let s = styles();
        let label = copy_label_spans(&s);
        let ctx = crate::test_support::draw_ctx(40, None);
        let plain = build_user_bubble(&user, false, &s, None).draw(&ctx);
        let bordered = build_user_bubble(&user, false, &s, Some(&label)).draw(&ctx);
        assert_eq!(
            plain.size, bordered.size,
            "the border reuses reserved padding, so no reflow"
        );

        // The bordered bubble draws the heavy corners and the copy hint. The
        // plain one has none of it.
        let grid = crate::test_support::flatten(&bordered);
        let last_col = usize::from(bordered.size.width) - 1;
        let last_row = usize::from(bordered.size.height) - 2; // above the spacer row
        assert_eq!(grid[0][0].char.grapheme(), "\u{250f}", "top-left corner");
        assert_eq!(grid[0][last_col].char.grapheme(), "\u{2513}", "top-right");
        assert_eq!(grid[last_row][0].char.grapheme(), "\u{2517}", "bottom-left");
        assert_eq!(
            grid[last_row][last_col].char.grapheme(),
            "\u{251b}",
            "bottom-right"
        );
        let key = default_action_shortcut(ACTION_COPY_MESSAGE).expect("copy chord bound");
        assert!(
            crate::test_support::rows(&bordered)[last_row].contains(&format!("{key} to copy")),
            "copy hint on the bottom edge: {:?}",
            crate::test_support::rows(&bordered),
        );

        let plain_grid = crate::test_support::flatten(&plain);
        assert!(
            plain_grid
                .iter()
                .flatten()
                .all(|c| c.char.grapheme() != "\u{250f}"),
            "the unfocused bubble draws no border",
        );
    }

    #[test]
    fn non_collapsible_long_message_is_never_folded() {
        let lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        let user = UserEntry {
            content: vec![UserContent::text(lines.join("\n"))],
            collapsible: false,
        };
        let r = bubble_rows(&user, false, 80);
        let body = r.join("\n");
        assert!(body.contains("line 29"), "{r:?}");
        assert!(!body.contains("to expand"), "{r:?}");
    }

    #[test]
    fn short_collapsible_notification_is_not_truncated() {
        let user = UserEntry {
            content: vec![UserContent::text(
                "<task-notification>\ntask #1 done\n</task-notification>",
            )],
            collapsible: true,
        };
        let r = bubble_rows(&user, false, 80);
        let body = r.join("\n");
        assert!(body.contains("task #1 done"), "{r:?}");
        assert!(!body.contains("to expand"), "{r:?}");
    }

    #[test]
    fn assistant_entry_renders_blocks_in_order_and_skips_tool_calls() {
        let t = transcript_with(EntryKind::Assistant(AssistantEntry {
            message: assistant_message(vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "pondering".into(),
                    thinking_signature: None,
                    redacted: false,
                }),
                // Tool calls render as their own transcript entry, so this
                // block is dropped from the markdown view.
                AssistantContent::ToolCall(aj_models::types::ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({}),
                }),
                AssistantContent::Text(TextContent {
                    text: "answer\n".into(),
                    text_signature: None,
                }),
            ]),
            finalized: true,
        }));
        let rows = assistant_markdown_rows(&t, false, 80);
        // Segments stack in order with one blank row between them and the
        // trailing spacer, the tool call contributing nothing.
        assert_eq!(rows, vec!["Thinking: pondering", "", "answer", ""]);
    }

    #[test]
    fn hidden_thinking_renders_placeholder() {
        let t = transcript_with(EntryKind::Assistant(AssistantEntry {
            message: assistant_message(vec![AssistantContent::Thinking(ThinkingContent {
                thinking: "secret".into(),
                thinking_signature: None,
                redacted: false,
            })]),
            finalized: true,
        }));
        let rows = assistant_markdown_rows(&t, true, 80);
        assert_eq!(rows, vec!["Thinking…", ""]);
    }

    #[test]
    fn redacted_thinking_renders_marker_even_when_expanded() {
        let t = transcript_with(EntryKind::Assistant(AssistantEntry {
            message: assistant_message(vec![AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: Some("opaque".into()),
                redacted: true,
            })]),
            finalized: true,
        }));
        let rows = assistant_markdown_rows(&t, false, 80);
        assert_eq!(rows, vec!["[Redacted thinking: ]", ""]);
    }

    #[test]
    fn vaxis_color_downsamples_rgb_only_in_color256_mode() {
        let rgb = ThemeRgb::Rgb(0x00, 0xd7, 0xff);
        // Truecolor keeps the 24-bit triple. Color256 folds it to the
        // shared `rgb_to_256` index (identical to the `aj` frontend).
        assert_eq!(
            vaxis_color(rgb, ColorMode::Truecolor),
            Color::Rgb([0x00, 0xd7, 0xff])
        );
        assert_eq!(
            vaxis_color(rgb, ColorMode::Color256),
            Color::Index(rgb_to_256(0x00, 0xd7, 0xff))
        );
        // Explicit palette indices and terminal-default pass through in
        // both modes.
        assert_eq!(
            vaxis_color(ThemeRgb::Ansi256(5), ColorMode::Color256),
            Color::Index(5)
        );
        assert_eq!(
            vaxis_color(ThemeRgb::Default, ColorMode::Color256),
            Color::Default
        );
    }

    #[test]
    fn notice_levels_pick_their_style() {
        let s = styles();
        for (level, style) in [
            (NoticeLevel::Info, s.dim),
            (NoticeLevel::Warning, s.warning),
            (NoticeLevel::Error, s.error),
        ] {
            let t = transcript_with(EntryKind::Notice(NoticeEntry {
                level,
                text: "note".into(),
            }));
            let spans = entry_spans(&t.entries()[0], &s);
            assert_eq!(spans[0].style, style);
        }
    }

    /// Notice and usage rows carry the one-column left inset that
    /// lines them up with the tool bubbles' content column.
    #[test]
    fn notice_and_usage_rows_are_inset_one_column() {
        let t = transcript_with(EntryKind::Notice(NoticeEntry {
            level: NoticeLevel::Info,
            text: "note".into(),
        }));
        let spans = entry_spans(&t.entries()[0], &styles());
        assert_eq!(joined(&spans), " note\n\n");

        let t = transcript_with(EntryKind::TurnUsage(aj_app::chat::TurnUsageEntry {
            agent_id: aj_agent::events::AgentId::Main,
            usage: aj_agent::types::TokenUsage {
                accumulated_input: 0,
                turn_input: 0,
                accumulated_output: 0,
                turn_output: 0,
                accumulated_cache_write: 0,
                turn_cache_write: 0,
                accumulated_cache_read: 0,
                turn_cache_read: 0,
            },
        }));
        let spans = entry_spans(&t.entries()[0], &styles());
        assert!(joined(&spans).starts_with(" Token Usage"), "{spans:?}");
    }

    /// The freed percentage rounds from the token delta and clamps at
    /// 0 when occupancy didn't drop.
    #[test]
    fn compaction_header_states_freed_percentage_and_clamps() {
        assert_eq!(
            compaction_header(100_000, 25_000),
            "Context compacted: 100k → 25k tokens (freed 75%)",
        );
        assert_eq!(
            compaction_header(1_000, 2_000),
            "Context compacted: 1.0k → 2.0k tokens (freed 0%)",
        );
        assert_eq!(
            compaction_header(0, 0),
            "Context compacted: 0 → 0 tokens (freed 0%)",
        );
    }

    fn compaction(summary: &str) -> Transcript {
        transcript_with(EntryKind::Compaction(CompactionEntry {
            tokens_before: 100_000,
            tokens_after: 25_000,
            summary: summary.into(),
        }))
    }

    #[test]
    fn collapsed_compaction_hides_summary_and_advertises_expand() {
        let t = compaction("secret summary body");
        let (rows, header) = compaction_view_rows(&t, false, 100);
        let text = rows.join("\n");
        assert!(text.contains("Context compacted"), "{text:?}");
        assert!(text.contains("(freed 75%)"), "{text:?}");
        assert!(text.contains("(Alt+O to expand)"), "{text:?}");
        assert!(!text.contains("secret summary body"), "{text:?}");
        assert!(rows[0].starts_with(' '), "one-column inset: {rows:?}");
        assert_eq!(header, styles().dim, "dim header");
    }

    /// With nothing to reveal there is nothing to advertise.
    #[test]
    fn collapsed_compaction_without_summary_has_no_expand_hint() {
        let t = compaction("");
        let (rows, _) = compaction_view_rows(&t, false, 100);
        assert!(!rows.join("\n").contains("to expand"), "{rows:?}");
    }

    #[test]
    fn expanded_compaction_shows_summary_after_a_blank_row() {
        let t = compaction("the full summary body");
        let (rows, _) = compaction_view_rows(&t, true, 100);
        let text = rows.join("\n");
        assert!(!text.contains("to expand"), "{text:?}");
        // Header, one blank separator row, then the markdown summary.
        assert!(rows[0].contains("tokens (freed 75%)"), "{rows:?}");
        assert_eq!(rows[1], "", "blank row between header and body: {rows:?}");
        assert!(rows[2].contains("the full summary body"), "{rows:?}");
    }

    /// A full draw over a populated model must not panic and must pin
    /// the tail while follow-tail is engaged.
    #[test]
    fn draw_renders_bottom_of_a_long_transcript() {
        let chat = chat_with_notices(50);
        let mut view = transcript_view(&chat);

        let surface = view.draw(&draw_ctx(40, 10));
        assert_eq!(surface.size.height, 10);
        assert!(view.follow_tail, "short draw at bottom keeps follow-tail");
        // The last visible child is the final entry (spacer included).
        assert!(view.list.borrow().is_at_bottom());
    }

    /// A chat model with `n` one-line notice rows (two transcript rows
    /// each, counting the spacer).
    fn chat_with_notices(n: usize) -> Rc<RefCell<ChatState>> {
        use aj_agent::events::AgentSettings;

        let mut chat = ChatState::new(
            AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            0,
            Arc::new(Vec::new()),
        );
        let mut lifecycle = aj_app::session::AgentLifecycle::default();
        for i in 0..n {
            let _ = aj_app::chat::reduce(
                &mut chat,
                &mut lifecycle,
                aj_agent::events::AgentEvent::Notice {
                    agent_id: aj_agent::events::AgentId::Main,
                    text: format!("row {i}"),
                },
            );
        }
        Rc::new(RefCell::new(chat))
    }

    fn transcript_view(chat: &Rc<RefCell<ChatState>>) -> TranscriptView {
        let theme = Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor);
        TranscriptView::new(
            Rc::clone(chat),
            &theme,
            Rc::new(std::cell::Cell::new(false)),
        )
    }

    /// A chat whose Main transcript alternates user and assistant messages:
    /// `user 0`, `assistant 0`, `user 1`, ..., for `n` user messages. The user
    /// entries land at the even indices (0, 2, 4, ...) with an assistant reply
    /// between them, so transcript-focus stepping must skip the assistant
    /// entries to move message to message.
    fn chat_with_user_messages(n: usize) -> Rc<RefCell<ChatState>> {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        for i in 0..n {
            apply(&chat, &mut life, user_end(&format!("user {i}")));
            apply(
                &chat,
                &mut life,
                assistant_message_end(text_message(&format!("assistant {i}"))),
            );
        }
        chat
    }

    fn mouse(col: i16, row: i16, kind: mouse::Type) -> Event {
        Event::Mouse(mouse::Mouse {
            col,
            row,
            xoffset: 0,
            yoffset: 0,
            button: mouse::Button::Left,
            mods: mouse::Modifiers::empty(),
            kind,
        })
    }

    /// The scrollbar thumb draws in the reserved last column only when
    /// the transcript overflows the viewport (Spec E section 1).
    #[test]
    fn thumb_draws_only_when_the_transcript_overflows() {
        // Fifty two-row entries overflow the 10-row viewport. Follow-tail
        // pins the viewport to the bottom, so the thumb sits at the bar's
        // lower end.
        let chat = chat_with_notices(50);
        let mut view = transcript_view(&chat);
        let surface = view.draw(&draw_ctx(40, 10));
        let grid = crate::test_support::flatten(&surface);
        let thumb_rows: Vec<usize> = (0..10)
            .filter(|&row| grid[row][39].char.grapheme() == "\u{2590}")
            .collect();
        assert_eq!(thumb_rows, vec![9], "thumb pinned to the bottom");

        // One entry fits: no thumb, a blank gutter column.
        let chat = chat_with_notices(1);
        let mut view = transcript_view(&chat);
        let surface = view.draw(&draw_ctx(40, 10));
        let grid = crate::test_support::flatten(&surface);
        for row in &grid {
            assert_eq!(row[39].char.grapheme(), " ");
        }
    }

    /// Dragging the thumb jumps the viewport and disengages
    /// follow-tail, and a drag that lands back at the bottom re-engages
    /// it, the same rule wheel scrolling follows (Spec E section 1).
    #[test]
    fn thumb_drag_disengages_follow_tail_until_back_at_bottom() {
        // An 11-row viewport over fifty two-row entries: the one-row
        // thumb sits at bar row 10 while follow-tail pins the bottom.
        let chat = chat_with_notices(50);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 11);
        let _ = view.draw(&ctx);
        assert!(view.follow_tail);

        // The bar column hit-tests to this view, so the press arrives
        // via handle_event and the bars grab it.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(39, 10, mouse::Type::Press));
        assert!(ec.consume_event, "the thumb grabbed the press");
        assert!(view.follow_tail, "a press alone does not disengage");

        // Drags over the content area target the inner list, so they
        // arrive via the capturing phase. Dragging to the top jumps the
        // viewport there and disengages follow-tail.
        let mut ec = EventContext::new();
        view.capture_event(&mut ec, &mouse(20, 0, mouse::Type::Drag));
        assert!(ec.consume_event, "the drag was intercepted");
        assert!(!view.follow_tail, "dragging disengages follow-tail");
        let surface = view.draw(&ctx);
        let rows = crate::test_support::rows(&surface);
        assert!(rows[0].contains("row 0"), "{rows:?}");
        assert!(!view.follow_tail, "not at the bottom, still disengaged");

        // Dragging back to the bar's end lands the viewport at the
        // bottom, which the post-draw check turns into re-engagement.
        let mut ec = EventContext::new();
        view.capture_event(&mut ec, &mouse(20, 10, mouse::Type::Drag));
        assert!(!view.follow_tail, "re-engage waits for the draw");
        let _ = view.draw(&ctx);
        assert!(view.follow_tail, "landing at the bottom re-engages");
        let mut ec = EventContext::new();
        view.capture_event(&mut ec, &mouse(20, 10, mouse::Type::Release));
        assert!(ec.consume_event, "the release ends the drag");
    }

    /// Wheel-up disengages follow-tail whether it arrives at this view
    /// (bar column, or direct forwarding) or in the capturing phase on
    /// its way to the inner list.
    #[test]
    fn wheel_up_disengages_follow_tail_in_both_phases() {
        let chat = chat_with_notices(50);
        let ctx = draw_ctx(40, 10);
        let wheel_up = Event::Mouse(mouse::Mouse {
            col: 20,
            row: 5,
            xoffset: 0,
            yoffset: 0,
            button: mouse::Button::WheelUp,
            mods: mouse::Modifiers::empty(),
            kind: mouse::Type::Press,
        });

        let mut view = transcript_view(&chat);
        let _ = view.draw(&ctx);
        assert!(view.follow_tail);
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &wheel_up);
        assert!(!view.follow_tail, "handle_event path disengages");

        let mut view = transcript_view(&chat);
        let _ = view.draw(&ctx);
        assert!(view.follow_tail);
        let mut ec = EventContext::new();
        view.capture_event(&mut ec, &wheel_up);
        assert!(!ec.consume_event, "the wheel still reaches the list");
        assert!(!view.follow_tail, "capture path disengages");
    }

    /// PageUp disengages follow-tail and scrolls the transcript up; paging
    /// back down to the bottom re-engages it (Spec E section 1).
    #[test]
    fn page_up_disengages_and_page_down_reengages_follow_tail() {
        // Fifty two-row entries over a 10-row viewport, so the transcript is
        // much taller than the page.
        let chat = chat_with_notices(50);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let _ = view.draw(&ctx);
        assert!(view.follow_tail, "opens following the tail");
        let bottom = crate::test_support::rows(&view.draw(&ctx));

        // Page up: follow-tail disengages and the viewport moves.
        view.page_up();
        assert!(!view.follow_tail, "paging up disengages follow-tail");
        let scrolled = crate::test_support::rows(&view.draw(&ctx));
        assert_ne!(scrolled, bottom, "the viewport moved up off the bottom");
        assert!(
            !view.list.borrow().is_at_bottom(),
            "no longer at the bottom"
        );
        assert!(!view.follow_tail, "still scrolled up, still disengaged");

        // Page back down to the bottom. A page turn that lands exactly on the
        // last row keeps the list's `has_more` set (it reports "at bottom"
        // only once a scroll overshoots the end, matching the wheel), so page
        // down until it re-engages, the way a user holding PageDown does.
        for _ in 0..4 {
            view.page_down();
            let _ = view.draw(&ctx);
            if view.follow_tail {
                break;
            }
        }
        assert!(
            view.follow_tail,
            "returning to the bottom re-engages follow-tail"
        );
        let returned = crate::test_support::rows(&view.draw(&ctx));
        assert_eq!(returned, bottom, "back at the bottom frame");
    }

    /// Editor-mode Home pins the viewport to the absolute top and disengages
    /// follow-tail, and End re-engages follow-tail so the viewport lands back
    /// at the bottom (Spec E section 1, the global Home/End chords).
    #[test]
    fn scroll_to_top_and_bottom_move_the_viewport_in_editor_mode() {
        let chat = chat_with_notices(50);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let _ = view.draw(&ctx);
        assert!(view.follow_tail, "opens following the tail");
        assert!(!view.in_focus_mode(), "editor mode: no item cursor");

        // Home jumps to the absolute top: the first row is item 0's first line
        // and follow-tail is off.
        view.scroll_to_top();
        assert!(!view.follow_tail, "Home disengages follow-tail");
        let rows = crate::test_support::rows(&view.draw(&ctx));
        assert!(rows[0].contains("row 0"), "top of the transcript: {rows:?}");
        assert_eq!(
            view.list.borrow().scroll_top(),
            0,
            "pinned to the first item"
        );
        assert_eq!(view.list.borrow().scroll_offset(), 0, "at its first line");
        assert!(
            !view.list.borrow().is_at_bottom(),
            "the top of a tall transcript is not the bottom"
        );

        // End re-engages follow-tail and the next draw lands back at the
        // bottom's last row.
        view.scroll_to_bottom();
        assert!(view.follow_tail, "End re-engages follow-tail");
        let rows = crate::test_support::rows(&view.draw(&ctx));
        assert!(view.list.borrow().is_at_bottom(), "back at the bottom");
        assert!(
            rows.join("\n").contains("row 49"),
            "shows the last row: {rows:?}"
        );
    }

    /// In focus mode the Home/End chords move the item cursor to the first /
    /// last user message rather than scrolling the viewport, matching the
    /// `g` / `G` jumps (Spec E section 1).
    #[test]
    fn scroll_to_top_and_bottom_move_the_item_cursor_in_focus_mode() {
        // User messages at indices 0, 2, 4, 6, 8; assistant replies between.
        let chat = chat_with_user_messages(5);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let _ = view.draw(&ctx);

        // Enter focus mode: the cursor lands on the last user message (index
        // 8), skipping the assistant reply after it.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &Event::FocusIn);
        let _ = view.draw(&ctx);
        assert!(view.in_focus_mode(), "focus mode is on");
        assert_eq!(
            view.list.borrow().cursor,
            8,
            "cursor on the last user message"
        );

        // Home moves the cursor to the first user message; follow-tail stays
        // off (the cursor owns the viewport in focus mode).
        view.scroll_to_top();
        assert_eq!(
            view.list.borrow().cursor,
            0,
            "Home moves to the first user message"
        );
        assert!(!view.follow_tail, "focus mode keeps follow-tail disengaged");

        // End moves the cursor back to the last user message.
        view.scroll_to_bottom();
        assert_eq!(
            view.list.borrow().cursor,
            8,
            "End moves to the last user message"
        );
        assert!(!view.follow_tail, "focus mode keeps follow-tail disengaged");
    }

    /// Switching the active view opens the switched-to view at its bottom
    /// with follow-tail engaged (Spec E section 1, per-view scroll). The host
    /// runs `set_active_view` then `reset_to_tail` on the switch; this drives
    /// that sequence from a scrolled-up main view and checks the switched-to
    /// view opens pinned to its own bottom.
    #[test]
    fn switching_active_view_reengages_follow_tail() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        // A tall main transcript.
        for i in 0..50 {
            apply(
                &chat,
                &mut life,
                AgentEvent::Notice {
                    agent_id: AgentId::Main,
                    text: format!("main row {i}"),
                },
            );
        }
        // A sub-agent with an equally tall transcript to switch to.
        spawn_sub(&chat, &mut life);
        for i in 0..50 {
            apply(
                &chat,
                &mut life,
                AgentEvent::Notice {
                    agent_id: AgentId::Sub(0),
                    text: format!("sub row {i}"),
                },
            );
        }

        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let _ = view.draw(&ctx);
        assert!(view.follow_tail);

        // Scroll the main view up so follow-tail is off; a plain redraw does
        // not re-engage it, so only an explicit reset can.
        view.page_up();
        let _ = view.draw(&ctx);
        assert!(!view.follow_tail, "paged up on the main view");
        let _ = view.draw(&ctx);
        assert!(!view.follow_tail, "a plain redraw does not re-engage");

        // The host's switch sequence: swap the model's view, then reset the
        // transcript to the tail so the switched-to view opens at its bottom.
        chat.borrow_mut().set_active_view(AgentId::Sub(0));
        view.reset_to_tail();
        let rows = crate::test_support::rows(&view.draw(&ctx));
        assert!(view.follow_tail, "reset_to_tail re-engages on the switch");
        assert!(
            view.list.borrow().is_at_bottom(),
            "at the sub view's bottom"
        );
        assert!(
            rows.join("\n").contains("sub row 49"),
            "shows the sub view's last row: {rows:?}"
        );
    }

    // ---- Transcript-focus mode (Spec E section 1) ------------------------

    fn key_press(codepoint: u32, mods: Modifiers) -> Event {
        Event::KeyPress(Key {
            codepoint,
            mods,
            ..Key::default()
        })
    }

    /// FocusIn enters focus mode and disengages follow-tail (so cursor
    /// navigation is not fought by auto-scroll). FocusOut leaves it again.
    /// The mode lives on the shared flag (`in_focus_mode`), not `draw_cursor`,
    /// which stays off in every mode (the border is the marker).
    #[test]
    fn focus_in_enters_focus_mode_and_disengages_follow_tail() {
        // User messages at indices 0, 2, 4, 6, 8; assistant replies between.
        let chat = chat_with_user_messages(5);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let _ = view.draw(&ctx);
        assert!(view.follow_tail, "opens following the tail");
        assert!(!view.in_focus_mode(), "not focused at construction");
        assert!(
            !view.list.borrow().draw_cursor,
            "the cursor gutter stays off"
        );

        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &Event::FocusIn);
        assert!(view.in_focus_mode(), "FocusIn enters focus mode");
        assert!(
            !view.list.borrow().draw_cursor,
            "focus mode does not turn the gutter on"
        );
        assert!(!view.follow_tail, "FocusIn disengages follow-tail");
        // The cursor lands on the last user message (index 8, not the trailing
        // assistant reply), and a draw keeps follow-tail off even though the
        // viewport sits at the bottom.
        let _ = view.draw(&ctx);
        assert!(!view.follow_tail, "focus mode keeps follow-tail disengaged");
        assert_eq!(
            view.list.borrow().cursor,
            8,
            "cursor on the last user message"
        );

        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &Event::FocusOut);
        assert!(!view.in_focus_mode(), "FocusOut leaves focus mode");
    }

    /// While focused, the step keys (arrows, j/k, Ctrl+P/N, Shift+Tab) move
    /// the cursor between user messages, skipping the assistant entries, and
    /// `g` / `G` jump to the first / last user message. The step keys clamp at
    /// the ends and are ignored while the transcript is not focused. (Tab and
    /// Home/End reach focus mode through the global chords, tested elsewhere.)
    #[test]
    fn nav_keys_step_between_user_messages_only_while_focused() {
        // User messages at indices 0, 2, 4, 6, 8; assistant replies between.
        let chat = chat_with_user_messages(5);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let _ = view.draw(&ctx);

        // Not focused: nav keys are ignored (no cursor, nothing moves).
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(Key::UP, Modifiers::empty()));
        assert!(!ec.consume_event, "unfocused: the key falls through");
        assert_eq!(view.list.borrow().cursor, 0, "unfocused: cursor unmoved");

        // Enter focus mode: the cursor lands on the last user message.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &Event::FocusIn);
        let _ = view.draw(&ctx);
        assert_eq!(view.list.borrow().cursor, 8);

        // Up / k / Ctrl+P step to the next-older user message, skipping the
        // assistant reply between (index 8 -> 6 -> 4 -> 2).
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(Key::UP, Modifiers::empty()));
        assert_eq!(
            view.list.borrow().cursor,
            6,
            "Up steps to the previous user message"
        );
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(u32::from('k'), Modifiers::empty()));
        assert_eq!(
            view.list.borrow().cursor,
            4,
            "k steps to the previous user message"
        );
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(u32::from('p'), Modifiers::CTRL));
        assert_eq!(
            view.list.borrow().cursor,
            2,
            "Ctrl+P steps to the previous user message"
        );

        // Down / j / Ctrl+N / Shift+Tab step to the next-newer user message.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(u32::from('j'), Modifiers::empty()));
        assert_eq!(
            view.list.borrow().cursor,
            4,
            "j steps to the next user message"
        );
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(Key::DOWN, Modifiers::empty()));
        assert_eq!(
            view.list.borrow().cursor,
            6,
            "Down steps to the next user message"
        );
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(Key::TAB, Modifiers::SHIFT));
        assert_eq!(
            view.list.borrow().cursor,
            8,
            "Shift+Tab steps to the next user message"
        );
        // At the last user message, stepping newer clamps.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(Key::DOWN, Modifiers::empty()));
        assert_eq!(view.list.borrow().cursor, 8, "Down clamps at the last");

        // g / G jump to the first / last user message.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(u32::from('g'), Modifiers::empty()));
        assert_eq!(
            view.list.borrow().cursor,
            0,
            "g jumps to the first user message"
        );
        // At the first user message, stepping older clamps.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(Key::UP, Modifiers::empty()));
        assert_eq!(view.list.borrow().cursor, 0, "Up clamps at the first");

        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(u32::from('g'), Modifiers::SHIFT));
        assert_eq!(
            view.list.borrow().cursor,
            8,
            "G jumps to the last user message"
        );
    }

    /// Esc while focused invokes the exit callback (the host wires it to
    /// refocus the editor). It is inert while the transcript is not focused.
    #[test]
    fn esc_invokes_the_exit_focus_callback_while_focused() {
        let chat = chat_with_notices(5);
        let mut view = transcript_view(&chat);
        let fired = Rc::new(std::cell::Cell::new(0u32));
        {
            let fired = Rc::clone(&fired);
            view.set_on_exit_focus(Box::new(move |_ctx| {
                fired.set(fired.get() + 1);
            }));
        }
        let ctx = draw_ctx(40, 10);
        let _ = view.draw(&ctx);

        // Unfocused: Esc is ignored, the callback does not fire.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(Key::ESCAPE, Modifiers::empty()));
        assert_eq!(fired.get(), 0, "unfocused Esc does not fire the callback");

        // Focus, then Esc fires the callback exactly once.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &Event::FocusIn);
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(Key::ESCAPE, Modifiers::empty()));
        assert_eq!(fired.get(), 1, "focused Esc fires the exit callback");
        assert!(ec.consume_event, "Esc is consumed");
    }

    /// Entering focus mode and navigating an empty transcript must not
    /// underflow the item index or panic. The user-message scan finds nothing,
    /// so every step is a no-op that leaves the cursor at 0.
    #[test]
    fn focus_mode_on_an_empty_transcript_is_safe() {
        let chat = chat_with_notices(0);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let _ = view.draw(&ctx);
        assert_eq!(view.entry_count(), 0, "the transcript is empty");
        assert!(!view.has_user_message(), "no user message to step to");

        // FocusIn lands the cursor on item 0 (there is nothing else to land
        // on) and a draw of the empty, focused list does not panic.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &Event::FocusIn);
        assert!(view.in_focus_mode(), "FocusIn enters focus mode");
        assert_eq!(
            view.list.borrow().cursor,
            0,
            "cursor clamps to 0 when empty"
        );
        let _ = view.draw(&ctx);

        // Every nav key is a clamped no-op on an empty list, none underflow.
        // Home/End reach focus mode through the global chord, so exercise the
        // methods it dispatches to rather than the widget's own keys.
        for k in [
            key_press(Key::UP, Modifiers::empty()),
            key_press(Key::DOWN, Modifiers::empty()),
            key_press(u32::from('g'), Modifiers::empty()),
            key_press(u32::from('g'), Modifiers::SHIFT),
        ] {
            let mut ec = EventContext::new();
            view.handle_event(&mut ec, &k);
            assert_eq!(view.list.borrow().cursor, 0, "cursor stays at 0 when empty");
        }
        view.scroll_to_top();
        assert_eq!(view.list.borrow().cursor, 0, "scroll_to_top stays at 0");
        view.scroll_to_bottom();
        assert_eq!(view.list.borrow().cursor, 0, "scroll_to_bottom stays at 0");
        let _ = view.draw(&ctx);
    }

    /// A notices-only transcript (splash / startup warnings, no user message)
    /// reports no user message, so the enter-focus chord (guarded on
    /// [`has_user_message`](TranscriptView::has_user_message)) does not engage.
    /// Even if focus were forced, landing on the last user message is a no-op.
    #[test]
    fn no_user_message_does_not_engage_focus() {
        let chat = chat_with_notices(5);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let _ = view.draw(&ctx);
        assert!(
            !view.has_user_message(),
            "notices are not user messages, so nothing to step to"
        );

        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &Event::FocusIn);
        let _ = view.draw(&ctx);
        assert_eq!(
            view.list.borrow().cursor,
            0,
            "no user message: landing is a no-op, cursor stays at 0"
        );
    }

    /// One user message: entering lands on it and prev/next are no-ops that
    /// keep the cursor on it (nothing older or newer to step to).
    #[test]
    fn one_user_message_prev_next_are_no_ops() {
        // Index 0: user, index 1: assistant reply.
        let chat = chat_with_user_messages(1);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let _ = view.draw(&ctx);
        assert!(view.has_user_message(), "the transcript has a user message");

        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &Event::FocusIn);
        let _ = view.draw(&ctx);
        assert_eq!(
            view.list.borrow().cursor,
            0,
            "lands on the only user message"
        );

        view.focus_prev_user_message();
        assert_eq!(view.list.borrow().cursor, 0, "nothing older to step to");
        view.focus_next_user_message();
        assert_eq!(view.list.borrow().cursor, 0, "nothing newer to step to");
    }

    /// The number of top-left corner glyphs in a drawn view: one per bordered
    /// (focused) bubble.
    fn border_count(view: &mut TranscriptView, ctx: &DrawContext) -> usize {
        crate::test_support::rows(&view.draw(ctx))
            .join("\n")
            .matches('\u{250f}')
            .count()
    }

    /// Focusing marks the newest user message with the border (and no other
    /// entry), stepping moves the border message-to-message, and leaving focus
    /// drops it. The transcript's row count never changes across any of it, so
    /// the marker never reflows the transcript (Spec E section 2).
    #[test]
    fn focus_border_marks_one_message_and_never_reflows() {
        // Users at 0, 2, 4, with assistant replies between. Tall viewport so
        // the whole transcript fits and the row comparison is exact.
        let chat = chat_with_user_messages(3);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(48, 40);

        let unfocused_rows = crate::test_support::rows(&view.draw(&ctx));
        assert_eq!(
            border_count(&mut view, &ctx),
            0,
            "no border while the editor is focused"
        );

        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &Event::FocusIn);
        let focused_rows = crate::test_support::rows(&view.draw(&ctx));
        assert_eq!(
            border_count(&mut view, &ctx),
            1,
            "exactly the focused message is bordered"
        );
        assert_eq!(
            focused_rows.len(),
            unfocused_rows.len(),
            "focusing does not change the transcript height",
        );
        // The focused entry is the newest user message. Its text still reads
        // "user 2" (the border sits in the padding, not over the content).
        assert!(
            focused_rows.iter().any(|r| r.contains("user 2")),
            "focused message content intact: {focused_rows:?}",
        );

        // Step older (Tab / Up): the border moves to the previous user message,
        // still exactly one, still the same total height.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(Key::UP, Modifiers::empty()));
        let stepped_rows = crate::test_support::rows(&view.draw(&ctx));
        assert_eq!(
            border_count(&mut view, &ctx),
            1,
            "still exactly one bordered message after stepping"
        );
        assert_eq!(
            stepped_rows.len(),
            unfocused_rows.len(),
            "stepping does not reflow the transcript",
        );

        // Leaving focus drops the border.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &Event::FocusOut);
        assert_eq!(
            border_count(&mut view, &ctx),
            0,
            "no border after leaving focus mode"
        );
    }

    /// `focused_message_text` returns the focused user message's own content
    /// while focused, and `None` otherwise.
    #[test]
    fn focused_message_text_reads_the_focused_user_message() {
        let chat = chat_with_user_messages(3);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(48, 40);
        let _ = view.draw(&ctx);

        assert_eq!(
            view.focused_message_text(),
            None,
            "no focused message while the editor is focused"
        );

        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &Event::FocusIn);
        let _ = view.draw(&ctx);
        assert_eq!(
            view.focused_message_text().as_deref(),
            Some("user 2"),
            "the newest user message's content"
        );

        // Step to the previous user message and re-read.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(Key::UP, Modifiers::empty()));
        assert_eq!(view.focused_message_text().as_deref(), Some("user 1"),);
    }

    // ---- Render cache: helpers -------------------------------------------

    fn cache_settings() -> aj_agent::events::AgentSettings {
        aj_agent::events::AgentSettings {
            provider: "scripted".into(),
            model_id: "scripted".into(),
            thinking: "off".into(),
            speed: "standard".into(),
            verbosity: "default".into(),
        }
    }

    fn empty_chat() -> Rc<RefCell<ChatState>> {
        Rc::new(RefCell::new(ChatState::new(
            cache_settings(),
            0,
            Arc::new(Vec::new()),
        )))
    }

    fn apply(chat: &Rc<RefCell<ChatState>>, life: &mut AgentLifecycle, event: AgentEvent) {
        let _ = reduce(&mut chat.borrow_mut(), life, event);
    }

    /// A caching builder over `chat` with a fresh cache and a concrete styles
    /// instance the uncached reference reuses, so cached and uncached renders
    /// are byte-comparable.
    fn caching_builder(chat: &Rc<RefCell<ChatState>>) -> EntryBuilder {
        let styles = Rc::new(styles());
        let copy_label = Rc::new(copy_label_spans(&styles));
        EntryBuilder {
            chat: Rc::clone(chat),
            styles,
            cache: Rc::new(RefCell::new(EntryRenderCache::new())),
            focus_mode: Rc::new(std::cell::Cell::new(false)),
            copy_label,
        }
    }

    /// Draw entry `idx` of the active view through the caching path, the way
    /// the list does: `item_at_idx` (computes the fingerprint) then draw
    /// (hit or miss).
    fn draw_cached(builder: &EntryBuilder, idx: usize, width: u16) -> Surface {
        let widget = builder.item_at_idx(idx, 0).expect("entry present");
        widget
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(width, None))
    }

    /// Like [`draw_cached`] but with an explicit `cursor`, so a test can move
    /// focus (`item_at_idx` folds `idx == cursor` into the fingerprint).
    fn draw_cached_cursor(
        builder: &EntryBuilder,
        idx: usize,
        cursor: usize,
        width: u16,
    ) -> Surface {
        let widget = builder.item_at_idx(idx, cursor).expect("entry present");
        widget
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(width, None))
    }

    /// Draw entry `idx` of `agent` with a fresh, uncached widget: the
    /// reference a cached render must match byte-for-byte.
    fn draw_uncached(builder: &EntryBuilder, agent: AgentId, idx: usize, width: u16) -> Surface {
        let chat = builder.chat.borrow();
        let entry = &chat.transcript(agent).expect("transcript").entries()[idx];
        let mut widget =
            build_entry_widget(entry, &chat, &builder.styles, false, None).into_boxed();
        widget.draw(&crate::test_support::draw_ctx(width, None))
    }

    /// Draw `idx` through the cache and assert the result matches a fresh
    /// uncached render of the same entry, proving the cached surface is not
    /// stale. Returns the cached surface.
    fn draw_and_assert_fresh(
        builder: &EntryBuilder,
        agent: AgentId,
        idx: usize,
        width: u16,
    ) -> Surface {
        let cached = draw_cached(builder, idx, width);
        let uncached = draw_uncached(builder, agent, idx, width);
        assert_same_surface(&cached, &uncached);
        cached
    }

    fn assert_same_surface(a: &Surface, b: &Surface) {
        assert_eq!(a.size, b.size, "surface sizes differ");
        assert_eq!(
            crate::test_support::flatten(a),
            crate::test_support::flatten(b),
            "composited cells differ (stale cache?)",
        );
    }

    fn misses(builder: &EntryBuilder) -> u64 {
        builder.cache.borrow().misses
    }

    fn hits(builder: &EntryBuilder) -> u64 {
        builder.cache.borrow().hits
    }

    fn text_message(text: &str) -> AssistantMessage {
        assistant_message(vec![AssistantContent::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })])
    }

    fn assistant_text_delta(text: &str) -> AgentEvent {
        AgentEvent::MessageUpdate {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::Assistant(assistant_message(Vec::new()))),
            event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: text.into(),
                partial: text_message(text),
            },
        }
    }

    fn assistant_message_end(message: AssistantMessage) -> AgentEvent {
        AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::Assistant(message)),
        }
    }

    fn user_end(text: &str) -> AgentEvent {
        AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::User(aj_models::types::UserMessage::text(text))),
        }
    }

    fn tool_start(agent: AgentId, call_id: &str, tool: &str) -> AgentEvent {
        AgentEvent::ToolExecutionStart {
            agent_id: agent,
            call_id: call_id.into(),
            tool: tool.into(),
            args: serde_json::json!({}),
        }
    }

    fn tool_end(agent: AgentId, call_id: &str, tool: &str, result: ToolDetails) -> AgentEvent {
        AgentEvent::ToolExecutionEnd {
            agent_id: agent,
            call_id: call_id.into(),
            tool: tool.into(),
            result,
            content: Vec::new().into(),
            is_error: false,
        }
    }

    fn bash(command: &str, stdout: &str, exit: Option<i32>, task_id: Option<usize>) -> ToolDetails {
        ToolDetails::Bash {
            command: command.into(),
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: exit,
            truncated: false,
            full_output_path: None,
            stdout_truncation: None,
            stderr_truncation: None,
            task_id,
        }
    }

    // ---- Render cache: effectiveness (hits happen) -----------------------

    /// Rendering an unchanged transcript twice records HITS on the second
    /// pass and no new misses, so the cache actually elides work.
    #[test]
    fn unchanged_transcript_hits_on_the_second_pass() {
        let chat = chat_with_notices(4);
        let builder = caching_builder(&chat);
        for i in 0..4 {
            draw_cached(&builder, i, 80);
        }
        let misses_after_first = misses(&builder);
        let hits_after_first = hits(&builder);
        assert_eq!(misses_after_first, 4, "first pass is all misses");
        for i in 0..4 {
            draw_cached(&builder, i, 80);
        }
        assert_eq!(misses(&builder), misses_after_first, "no new misses");
        assert_eq!(hits(&builder), hits_after_first + 4, "second pass all hits");
    }

    /// A width change is a per-slot miss even when the fingerprint is
    /// unchanged, and the re-rendered surface matches a fresh uncached draw
    /// at the new width.
    #[test]
    fn width_change_forces_a_per_slot_miss() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(&chat, &mut life, tool_start(AgentId::Main, "c1", "bash"));
        apply(
            &chat,
            &mut life,
            tool_end(
                AgentId::Main,
                "c1",
                "bash",
                bash("echo hi", "hi\n", Some(0), None),
            ),
        );
        let builder = caching_builder(&chat);
        draw_cached(&builder, 0, 80);
        draw_cached(&builder, 0, 80);
        assert_eq!(misses(&builder), 1, "same width hits");
        assert_eq!(hits(&builder), 1);
        let narrow = draw_and_assert_fresh(&builder, AgentId::Main, 0, 40);
        assert_eq!(misses(&builder), 2, "width change missed");
        assert_eq!(
            usize::from(narrow.size.width),
            40,
            "re-rendered at the new width"
        );
    }

    // ---- Render cache: per-kind no-stale ---------------------------------

    /// Assistant text growth changes the fingerprint, so the second render
    /// misses and matches a fresh uncached draw of the grown message.
    #[test]
    fn assistant_text_delta_growth_is_not_stale() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(&chat, &mut life, assistant_text_delta("Hel"));
        let builder = caching_builder(&chat);
        let first = draw_and_assert_fresh(&builder, AgentId::Main, 0, 80);

        apply(&chat, &mut life, assistant_text_delta("Hello world"));
        let grown = draw_and_assert_fresh(&builder, AgentId::Main, 0, 80);
        assert_eq!(misses(&builder), 2, "growth forced a rebuild");
        assert_ne!(
            crate::test_support::flatten(&first),
            crate::test_support::flatten(&grown),
            "the render actually changed",
        );
        assert!(
            crate::test_support::rows(&grown)
                .join("\n")
                .contains("Hello world"),
            "grown text rendered",
        );
    }

    /// Finalizing an assistant entry flips `finalized`, which the fingerprint
    /// tracks, so the post-finalize render is fresh.
    #[test]
    fn assistant_finalize_is_not_stale() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(&chat, &mut life, assistant_text_delta("Answer"));
        let builder = caching_builder(&chat);
        draw_and_assert_fresh(&builder, AgentId::Main, 0, 80);

        apply(
            &chat,
            &mut life,
            assistant_message_end(text_message("Answer")),
        );
        draw_and_assert_fresh(&builder, AgentId::Main, 0, 80);
        assert_eq!(misses(&builder), 2, "finalize forced a rebuild");
    }

    /// A tool cell walking pending -> details -> done stays fresh at each
    /// step (status, details presence, and payload all ride the fingerprint).
    #[test]
    fn tool_pending_details_done_is_not_stale() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(&chat, &mut life, tool_start(AgentId::Main, "c1", "bash"));
        let builder = caching_builder(&chat);
        // Pending: header only, no body.
        draw_and_assert_fresh(&builder, AgentId::Main, 0, 60);
        // Done with a result body.
        apply(
            &chat,
            &mut life,
            tool_end(
                AgentId::Main,
                "c1",
                "bash",
                bash("echo hi", "hi\n", Some(0), None),
            ),
        );
        let done = draw_and_assert_fresh(&builder, AgentId::Main, 0, 60);
        assert_eq!(misses(&builder), 2, "details+status change rebuilt");
        assert!(
            crate::test_support::rows(&done)
                .join("\n")
                .contains("[exit 0]"),
            "result body rendered",
        );
    }

    /// Streaming bash stdout growth (the size proxy) changes the fingerprint,
    /// so the grown output is not served stale.
    #[test]
    fn tool_streaming_output_growth_is_not_stale() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(&chat, &mut life, tool_start(AgentId::Main, "c1", "bash"));
        apply(
            &chat,
            &mut life,
            tool_end(
                AgentId::Main,
                "c1",
                "bash",
                bash("run", "line 1\n", Some(0), None),
            ),
        );
        let builder = caching_builder(&chat);
        let first = draw_and_assert_fresh(&builder, AgentId::Main, 0, 60);

        // The result payload grows (a later TaskOutput-style update).
        apply(
            &chat,
            &mut life,
            AgentEvent::ToolExecutionUpdate {
                agent_id: AgentId::Main,
                call_id: "c1".into(),
                tool: "bash".into(),
                args: serde_json::json!({}),
                partial: bash("run", "line 1\nline 2\nline 3\n", Some(0), None),
                content: Vec::new().into(),
            },
        );
        let grown = draw_and_assert_fresh(&builder, AgentId::Main, 0, 60);
        assert_eq!(misses(&builder), 2, "output growth rebuilt");
        assert_ne!(
            crate::test_support::flatten(&first),
            crate::test_support::flatten(&grown),
            "the render actually changed",
        );
    }

    /// A background task's terminal status changes the cell's badge and tint,
    /// which the fingerprint tracks through the task-status field.
    #[test]
    fn tool_task_status_change_is_not_stale() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(&chat, &mut life, tool_start(AgentId::Main, "c1", "bash"));
        apply(
            &chat,
            &mut life,
            AgentEvent::TaskStart {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "c1".into(),
                kind: TaskKind::Bash {
                    command: "sleep 1".into(),
                },
                label: "sleep 1".into(),
            },
        );
        apply(
            &chat,
            &mut life,
            tool_end(
                AgentId::Main,
                "c1",
                "bash",
                bash("sleep 1", "", None, Some(1)),
            ),
        );
        let builder = caching_builder(&chat);
        let running = draw_and_assert_fresh(&builder, AgentId::Main, 0, 60);
        assert!(
            crate::test_support::rows(&running)
                .join("\n")
                .contains("[task #1]"),
            "running badge",
        );

        apply(
            &chat,
            &mut life,
            AgentEvent::TaskEnd {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "c1".into(),
                status: TaskStatus::Exited(Some(0)),
                label: "sleep 1".into(),
            },
        );
        let done = draw_and_assert_fresh(&builder, AgentId::Main, 0, 60);
        assert_eq!(misses(&builder), 2, "task status change rebuilt");
        assert!(
            crate::test_support::rows(&done)
                .join("\n")
                .contains("exited 0"),
            "terminal badge rendered: {:?}",
            crate::test_support::rows(&done),
        );
    }

    /// The fingerprint distinguishes user-entry content and the collapsible
    /// flag, so a slot could never serve one user render for another. User
    /// entries are immutable after append, so this fingerprint sensitivity is
    /// the anti-stale guarantee for them.
    #[test]
    fn user_entry_fingerprint_tracks_content_and_collapsible() {
        let hello = transcript_with(EntryKind::User(UserEntry {
            content: vec![UserContent::text("hello")],
            collapsible: false,
        }));
        let longer = transcript_with(EntryKind::User(UserEntry {
            content: vec![UserContent::text("hello, world")],
            collapsible: false,
        }));
        let collapsible = transcript_with(EntryKind::User(UserEntry {
            content: vec![UserContent::text("hello")],
            collapsible: true,
        }));
        let chat = empty_chat();
        let fp = |t: &Transcript| entry_fingerprint(&t.entries()[0], &chat.borrow());
        assert_ne!(fp(&hello), fp(&longer), "content length is fingerprinted");
        assert_ne!(fp(&hello), fp(&collapsible), "collapsible is fingerprinted");
    }

    /// A user entry renders through the cache and hits when unchanged.
    #[test]
    fn user_entry_hits_when_unchanged() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(
            &chat,
            &mut life,
            AgentEvent::MessageEnd {
                agent_id: AgentId::Main,
                message: AgentMessage::wire(Message::User(aj_models::types::UserMessage::text(
                    "hi there",
                ))),
            },
        );
        let builder = caching_builder(&chat);
        draw_and_assert_fresh(&builder, AgentId::Main, 0, 60);
        draw_cached(&builder, 0, 60);
        assert_eq!(misses(&builder), 1, "second draw hit");
        assert_eq!(hits(&builder), 1);
    }

    /// Moving focus re-renders exactly the entry gaining and the entry losing
    /// the border, and leaves the rest cache hits, because `focused` is folded
    /// into the fingerprint (Spec E section 2).
    #[test]
    fn moving_focus_rerenders_only_the_two_affected_user_entries() {
        // Users at 0 and 2, assistant replies at 1 and 3.
        let chat = chat_with_user_messages(2);
        let builder = caching_builder(&chat);
        builder.focus_mode.set(true);
        let width = 60;

        // First pass with the cursor on the last user message (index 2): every
        // entry is a fresh build.
        for idx in 0..4 {
            draw_cached_cursor(&builder, idx, 2, width);
        }
        assert_eq!(misses(&builder), 4, "first pass builds every entry");
        assert_eq!(hits(&builder), 0);

        // Move focus to the first user message (index 0): only the two user
        // entries change border state. The assistant entries are unaffected.
        for idx in 0..4 {
            draw_cached_cursor(&builder, idx, 0, width);
        }
        assert_eq!(misses(&builder), 6, "only the two user entries rebuilt");
        assert_eq!(hits(&builder), 2, "the two assistant entries hit");
    }

    /// The fingerprint tracks a compaction entry's summary length and both
    /// token counts. Compaction entries are immutable after append, so this
    /// is their anti-stale guarantee.
    #[test]
    fn compaction_fingerprint_tracks_summary_and_tokens() {
        let base = transcript_with(EntryKind::Compaction(CompactionEntry {
            tokens_before: 100_000,
            tokens_after: 25_000,
            summary: "one".into(),
        }));
        let other_summary = transcript_with(EntryKind::Compaction(CompactionEntry {
            tokens_before: 100_000,
            tokens_after: 25_000,
            summary: "one two".into(),
        }));
        let other_tokens = transcript_with(EntryKind::Compaction(CompactionEntry {
            tokens_before: 90_000,
            tokens_after: 25_000,
            summary: "one".into(),
        }));
        let chat = empty_chat();
        let fp = |t: &Transcript| entry_fingerprint(&t.entries()[0], &chat.borrow());
        assert_ne!(
            fp(&base),
            fp(&other_summary),
            "summary length fingerprinted"
        );
        assert_ne!(fp(&base), fp(&other_tokens), "token counts fingerprinted");
    }

    // ---- Render cache: sub-agent box -------------------------------------

    /// Spawn a sub-agent with one assistant line in its child transcript, and
    /// return the (Main) index of the box entry (always 0 here).
    fn spawn_sub(chat: &Rc<RefCell<ChatState>>, life: &mut AgentLifecycle) {
        apply(
            chat,
            life,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(0),
                task: "scout the code".into(),
                background: false,
                settings: cache_settings(),
            },
        );
        apply(
            chat,
            life,
            AgentEvent::MessageEnd {
                agent_id: AgentId::Sub(0),
                message: AgentMessage::wire(Message::Assistant(text_message("starting"))),
            },
        );
    }

    /// Appending a new child entry to the sub transcript changes the box's
    /// fingerprint (child count + fold), so the box render is not stale.
    #[test]
    fn subagent_child_append_is_not_stale() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        spawn_sub(&chat, &mut life);
        let builder = caching_builder(&chat);
        draw_and_assert_fresh(&builder, AgentId::Main, 0, 70);

        apply(
            &chat,
            &mut life,
            AgentEvent::Notice {
                agent_id: AgentId::Sub(0),
                text: "sub-child-marker".into(),
            },
        );
        let after = draw_and_assert_fresh(&builder, AgentId::Main, 0, 70);
        assert_eq!(misses(&builder), 2, "child append rebuilt the box");
        assert!(
            crate::test_support::rows(&after)
                .join("\n")
                .contains("sub-child-marker"),
            "appended child rendered inside the box",
        );
    }

    /// Growth of the sub's tail child (a streaming assistant line) changes the
    /// box fingerprint through the child fold.
    #[test]
    fn subagent_last_child_streaming_is_not_stale() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        spawn_sub(&chat, &mut life);
        let builder = caching_builder(&chat);
        let first = draw_and_assert_fresh(&builder, AgentId::Main, 0, 70);

        // Replace the sub's single assistant line with a longer one.
        apply(
            &chat,
            &mut life,
            AgentEvent::MessageEnd {
                agent_id: AgentId::Sub(0),
                message: AgentMessage::wire(Message::Assistant(text_message(
                    "starting the investigation now",
                ))),
            },
        );
        let grown = draw_and_assert_fresh(&builder, AgentId::Main, 0, 70);
        assert_eq!(misses(&builder), 2, "tail-child growth rebuilt the box");
        assert_ne!(
            crate::test_support::flatten(&first),
            crate::test_support::flatten(&grown),
            "the box render actually changed",
        );
    }

    /// A background task updating a NON-tail child (its badge flips on
    /// `TaskEnd`) changes the box fingerprint. This only holds because the
    /// fold covers every child, not just the tail.
    #[test]
    fn subagent_non_tail_child_update_is_not_stale() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(
            &chat,
            &mut life,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(0),
                task: "scout".into(),
                background: false,
                settings: cache_settings(),
            },
        );
        // A bash tool cell with a background task, then a trailing notice, so
        // the tool cell is a NON-tail child.
        apply(&chat, &mut life, tool_start(AgentId::Sub(0), "c1", "bash"));
        apply(
            &chat,
            &mut life,
            AgentEvent::TaskStart {
                agent_id: AgentId::Sub(0),
                task_id: 1,
                call_id: "c1".into(),
                kind: TaskKind::Bash {
                    command: "sleep 1".into(),
                },
                label: "sleep 1".into(),
            },
        );
        apply(
            &chat,
            &mut life,
            tool_end(
                AgentId::Sub(0),
                "c1",
                "bash",
                bash("sleep 1", "", None, Some(1)),
            ),
        );
        apply(
            &chat,
            &mut life,
            AgentEvent::Notice {
                agent_id: AgentId::Sub(0),
                text: "tail notice".into(),
            },
        );
        let builder = caching_builder(&chat);
        let running = draw_and_assert_fresh(&builder, AgentId::Main, 0, 70);
        assert!(
            crate::test_support::rows(&running)
                .join("\n")
                .contains("[task #1]"),
            "running badge on the non-tail child: {:?}",
            crate::test_support::rows(&running),
        );

        // Terminal status flips the non-tail child's badge.
        apply(
            &chat,
            &mut life,
            AgentEvent::TaskEnd {
                agent_id: AgentId::Sub(0),
                task_id: 1,
                call_id: "c1".into(),
                status: TaskStatus::Exited(Some(0)),
                label: "sleep 1".into(),
            },
        );
        let done = draw_and_assert_fresh(&builder, AgentId::Main, 0, 70);
        assert_eq!(misses(&builder), 2, "non-tail child change rebuilt the box");
        assert!(
            crate::test_support::rows(&done)
                .join("\n")
                .contains("exited 0"),
            "non-tail child badge updated: {:?}",
            crate::test_support::rows(&done),
        );
    }

    // ---- Render cache: global clears -------------------------------------

    /// A chat with an assistant line and a done tool cell, wrapped for a
    /// full `TranscriptView` draw.
    fn chat_with_tool() -> Rc<RefCell<ChatState>> {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(&chat, &mut life, assistant_text_delta("hi"));
        apply(&chat, &mut life, assistant_message_end(text_message("hi")));
        apply(
            &chat,
            &mut life,
            tool_start(AgentId::Main, "c1", "read_file"),
        );
        apply(
            &chat,
            &mut life,
            tool_end(
                AgentId::Main,
                "c1",
                "read_file",
                ToolDetails::Text {
                    summary: "/tmp/x".into(),
                    body: (1..=20)
                        .map(|i| format!("line {i}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                },
            ),
        );
        chat
    }

    /// Toggling `tools_expanded` clears the whole cache, forcing a full miss
    /// on the next draw.
    #[test]
    fn toggling_tools_expanded_clears_the_cache() {
        let chat = chat_with_tool();
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(60, 24);
        let _ = view.draw(&ctx);
        let _ = view.draw(&ctx);
        let hits_before = view.cache.borrow().hits;
        let misses_before = view.cache.borrow().misses;
        assert!(hits_before > 0, "second draw hit");

        chat.borrow_mut().tools_expanded = true;
        let _ = view.draw(&ctx);
        assert!(
            view.cache.borrow().misses > misses_before,
            "toggling tools_expanded forced misses",
        );
    }

    /// Toggling `hide_thinking_block` clears the whole cache.
    #[test]
    fn toggling_hide_thinking_clears_the_cache() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(
            &chat,
            &mut life,
            AgentEvent::MessageUpdate {
                agent_id: AgentId::Main,
                message: AgentMessage::wire(Message::Assistant(assistant_message(Vec::new()))),
                event: AssistantMessageEvent::ThinkingDelta {
                    content_index: 0,
                    delta: "pondering".into(),
                    partial: assistant_message(vec![AssistantContent::Thinking(ThinkingContent {
                        thinking: "pondering".into(),
                        thinking_signature: None,
                        redacted: false,
                    })]),
                },
            },
        );
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(60, 24);
        let _ = view.draw(&ctx);
        let _ = view.draw(&ctx);
        let misses_before = view.cache.borrow().misses;
        assert!(view.cache.borrow().hits > 0, "second draw hit");

        chat.borrow_mut().hide_thinking_block = true;
        let _ = view.draw(&ctx);
        assert!(
            view.cache.borrow().misses > misses_before,
            "toggling hide_thinking_block forced misses",
        );
        let rows = crate::test_support::rows(&view.draw(&ctx));
        assert!(
            rows.join("\n").contains("Thinking…"),
            "placeholder shown: {rows:?}"
        );
    }

    /// Toggling `syntax_highlight` clears the whole cache, so a live change
    /// re-renders code blocks with the new coloring.
    #[test]
    fn toggling_syntax_highlight_clears_the_cache() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(
            &chat,
            &mut life,
            assistant_text_delta("```rust\nlet y = 2;\n```"),
        );
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(60, 24);
        let _ = view.draw(&ctx);
        let _ = view.draw(&ctx);
        let misses_before = view.cache.borrow().misses;
        assert!(view.cache.borrow().hits > 0, "second draw hit");

        // Seeded off (the `ChatState` default), so flip it on.
        chat.borrow_mut().syntax_highlight = true;
        let _ = view.draw(&ctx);
        assert!(
            view.cache.borrow().misses > misses_before,
            "toggling syntax_highlight forced misses",
        );
    }

    /// A display toggle drops the per-entry text cache, not just the render
    /// cache. The text cache backs a live selection's highlight and copy, so a
    /// stale hit after a toggle would read the pre-toggle wrapping.
    #[test]
    fn display_toggle_drops_the_entry_text_cache() {
        let chat = chat_with_tool();
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(60, 24);
        let _ = view.draw(&ctx);
        let w = view.content_width();
        // Warm the text cache the way a selection would.
        let _ = view.entry_rows(entry_id(&chat, 0), w);
        assert!(!view.entry_text.slots.is_empty(), "text cache warmed");

        chat.borrow_mut().tools_expanded = true;
        let _ = view.draw(&ctx);
        assert!(
            view.entry_text.slots.is_empty(),
            "a display toggle must drop the per-entry text cache",
        );
    }

    /// Switching the active view clears the whole cache.
    #[test]
    fn switching_active_view_clears_the_cache() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        spawn_sub(&chat, &mut life);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(60, 24);
        let _ = view.draw(&ctx);
        let _ = view.draw(&ctx);
        let misses_before = view.cache.borrow().misses;
        assert!(view.cache.borrow().hits > 0, "second draw hit");

        chat.borrow_mut().set_active_view(AgentId::Sub(0));
        let _ = view.draw(&ctx);
        assert!(
            view.cache.borrow().misses > misses_before,
            "switching the active view forced misses",
        );
    }

    /// A theme swap through `set_styles` clears the cache.
    #[test]
    fn set_styles_clears_the_cache() {
        let chat = chat_with_notices(2);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(60, 24);
        let _ = view.draw(&ctx);
        assert!(!view.cache.borrow().slots.is_empty(), "cache populated");
        view.set_styles(Rc::new(styles()));
        assert!(
            view.cache.borrow().slots.is_empty(),
            "set_styles cleared it"
        );
    }

    /// A session rebuild reuses the `chat` cell but installs a fresh session
    /// whose transcript restarts `EntryId` at 0. With same-length content the
    /// new entry's fingerprint collides with the cached slot, and the globals
    /// are unchanged, so only the `reset_to_tail` clear stops the previous
    /// session's surface from being replayed. Without that clear this test
    /// reads the stale "hello".
    #[test]
    fn session_rebuild_does_not_serve_the_previous_sessions_surface() {
        // The shell holds `chat` by identity across a swap; the view shares it.
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(&chat, &mut life, user_end("hello"));
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(60, 24);
        let _ = view.draw(&ctx);
        // Second draw hits the cache, so the (Main, EntryId(0)) slot is warm.
        let first = crate::test_support::rows(&view.draw(&ctx));
        assert!(
            first.join("\n").contains("hello"),
            "first session: {first:?}"
        );
        assert!(view.cache.borrow().hits > 0, "first session slot cached");

        // Swap in a fresh session in place. Its first entry reuses EntryId(0)
        // with different, same-length content ("world" vs "hello"), so the
        // fingerprint collides, and its globals (Main, false, false) match the
        // outgoing session's, so the draw-time global clear does not fire.
        {
            let mut fresh = ChatState::new(cache_settings(), 0, Arc::new(Vec::new()));
            let mut fresh_life = AgentLifecycle::default();
            let _ = reduce(&mut fresh, &mut fresh_life, user_end("world"));
            *chat.borrow_mut() = fresh;
        }
        // The rebind hook the shell runs on install.
        view.reset_to_tail();

        let rows = crate::test_support::rows(&view.draw(&ctx)).join("\n");
        assert!(
            rows.contains("world") && !rows.contains("hello"),
            "fresh session content, not the previous session's cached surface: {rows:?}",
        );
    }

    // ---- Per-entry text layout (Spec E section 2) ------------------------

    /// The `EntryId` of the active view's entry at `idx`, for building
    /// `SelPos` values in the selection tests.
    fn entry_id(chat: &Rc<RefCell<ChatState>>, idx: usize) -> EntryId {
        let chat = chat.borrow();
        chat.transcript(chat.active_view())
            .expect("transcript")
            .entries()[idx]
            .id
    }

    /// The alignment guarantee: the per-entry provider wraps byte-identically
    /// to the visible `ListView`. Draw the view into a viewport taller than the
    /// content (so the list shows every line from the top) and assert each
    /// entry's `entry_rows` equals the cells that entry occupies in the
    /// composed frame. This is the property select-to-copy depends on.
    #[test]
    fn entry_rows_match_the_visible_render() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(
            &chat,
            &mut life,
            user_end("the quick brown fox jumps over the lazy dog"),
        );
        apply(
            &chat,
            &mut life,
            assistant_message_end(text_message(
                "a reply long enough to wrap across several rows at this width",
            )),
        );
        let mut view = transcript_view(&chat);
        let (vw, vh) = (20u16, 40u16);

        // Drawing lays entries out through the `ListView` and stashes the cell
        // size / width method the per-entry provider reuses. A viewport taller
        // than the content top-anchors it at row 0.
        let surface = view.draw(&draw_ctx(vw, vh));
        let grid = crate::test_support::flatten(&surface);

        // The bars reserve the last column and the list draws entries at col 0,
        // so entries wrap at content width vw - 1.
        let content_w = vw - 1;
        let id0 = entry_id(&chat, 0);
        let id1 = entry_id(&chat, 1);
        let rows0 = view.entry_rows(id0, content_w);
        let rows1 = view.entry_rows(id1, content_w);
        assert!(rows0.len() > 1 && rows1.len() > 1, "both entries wrapped");
        assert!(
            rows0.len() + rows1.len() < usize::from(vh),
            "content fits, so the list top-anchors it at row 0",
        );

        // Entry 0 occupies the first `rows0.len()` screen rows, entry 1 the
        // ones right after it.
        for (r, line) in rows0.iter().enumerate() {
            assert_eq!(
                &grid[r][..usize::from(content_w)],
                line.as_slice(),
                "entry 0 row {r} differs from the visible render",
            );
        }
        let base = rows0.len();
        for (r, line) in rows1.iter().enumerate() {
            assert_eq!(
                &grid[base + r][..usize::from(content_w)],
                line.as_slice(),
                "entry 1 row {r} differs from the visible render",
            );
        }
        // The reserved scrollbar column stays blank while the transcript fits.
        for r in 0..(rows0.len() + rows1.len()) {
            assert_eq!(grid[r][usize::from(content_w)], Cell::default());
        }
    }

    // ---- Free-form selection (Spec E section 2) --------------------------

    /// Rows any cell of which carries the selection background.
    fn highlighted_rows(grid: &[Vec<Cell>], bg: Color) -> Vec<usize> {
        grid.iter()
            .enumerate()
            .filter(|(_, row)| row.iter().any(|cell| cell.style.bg == bg))
            .map(|(r, _)| r)
            .collect()
    }

    /// A selection over a known entry range extracts exactly the copied string
    /// (the range the release hands to OSC 52).
    #[test]
    fn selection_extracts_the_copied_text() {
        let chat = chat_with_notices(3);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        // A draw stashes the cell size / width method the provider lays out
        // under.
        let _ = view.draw(&ctx);
        let w = view.content_width();

        // " row 0" from col 1 of entry 0, through " row 1" up to col 6 of
        // entry 1: the notice line, its blank spacer, then the next notice.
        let anchor = SelPos {
            entry: entry_id(&chat, 0),
            line: 0,
            col: 1,
        };
        let caret = SelPos {
            entry: entry_id(&chat, 1),
            line: 0,
            col: 6,
        };
        view.selection = Some(Selection { anchor, caret });
        assert_eq!(view.extract_selection(w, anchor, caret), "row 0\n\n row 1");
    }

    /// One row of single-width cells from `s`, for the row-range reader test.
    fn cells(s: &str) -> Vec<Cell> {
        s.chars()
            .map(|c| Cell {
                char: Character::new(c.to_string(), 1),
                ..Cell::default()
            })
            .collect()
    }

    /// The per-row range reader that both extraction and (indirectly) the
    /// highlight rest on: it normalizes reversed endpoints, clamps out-of-range
    /// rows and columns, trims trailing pad per line, and joins rows with `\n`.
    /// This is the panic-safety net when a stale selection outlives a width or
    /// content change, so it is exercised directly.
    #[test]
    fn extract_from_lines_reads_normalized_ranges() {
        let lines = vec![cells(" row 0"), cells(""), cells(" row 1")];
        // Forward substring within a line, and the same range reversed.
        assert_eq!(extract_from_lines(&lines, (0, 1), (0, 4)), "row");
        assert_eq!(extract_from_lines(&lines, (0, 4), (0, 1)), "row");
        // A column past the content trims the trailing pad.
        assert_eq!(extract_from_lines(&lines, (0, 0), (0, 40)), " row 0");
        // Multi-line join keeps the blank middle row.
        assert_eq!(
            extract_from_lines(&lines, (0, 1), (2, 6)),
            "row 0\n\n row 1"
        );
        // An out-of-range end row clamps to the last line.
        assert_eq!(
            extract_from_lines(&lines, (0, 1), (99, 3)),
            "row 0\n\n row 1"
        );
        // A degenerate range is empty.
        assert_eq!(extract_from_lines(&lines, (1, 0), (1, 0)), "");
    }

    /// A selection spanning two entries highlights the tail of the start row
    /// from `min.col`, the whole interior rows (including an entry's blank
    /// spacer), and the head of the end row up to `max.col`.
    #[test]
    fn selection_highlights_span_multiple_entries() {
        let chat = chat_with_notices(20);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let bg = view.styles.selection_bg;

        // Anchor to the top so entry 0 is at screen rows 0..=1 and entry 1 at
        // rows 2..=3 (each notice is its line plus a blank spacer).
        view.follow_tail = false;
        view.list.borrow_mut().scroll_lines(-1000);
        let _ = view.draw(&ctx);

        view.selection = Some(Selection {
            anchor: SelPos {
                entry: entry_id(&chat, 0),
                line: 0,
                col: 3,
            },
            caret: SelPos {
                entry: entry_id(&chat, 1),
                line: 0,
                col: 2,
            },
        });
        let surface = view.draw(&ctx);
        let grid = crate::test_support::flatten(&surface);

        assert_eq!(highlighted_rows(&grid, bg), vec![0, 1, 2]);
        // Start row: highlighted from min.col, not before it.
        assert_ne!(grid[0][2].style.bg, bg, "before min.col is untouched");
        assert_eq!(grid[0][3].style.bg, bg, "min.col starts the highlight");
        // Interior row (entry 0's blank spacer): highlighted end to end.
        assert_eq!(grid[1][0].style.bg, bg, "interior row painted at col 0");
        assert_eq!(grid[1][38].style.bg, bg, "interior row painted to the edge");
        // End row: highlighted up to max.col, not past it.
        assert_eq!(grid[2][1].style.bg, bg, "before max.col is highlighted");
        assert_ne!(grid[2][2].style.bg, bg, "max.col ends the highlight");
    }

    /// A drawn selection highlights only the covered cells (the background is
    /// restyled, the text preserved) and leaves every other cell untouched.
    #[test]
    fn selection_highlights_only_the_covered_cells() {
        let chat = chat_with_notices(20);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let bg = view.styles.selection_bg;

        // Top of the transcript: entry 0 fills screen rows 0..=1, so entry 1
        // (" row 1") starts at screen row 2.
        view.follow_tail = false;
        view.list.borrow_mut().scroll_lines(-1000);
        let _ = view.draw(&ctx);

        // Highlight " row 1" (entry 1, its first line) content cols [0, 6).
        view.selection = Some(Selection {
            anchor: SelPos {
                entry: entry_id(&chat, 1),
                line: 0,
                col: 0,
            },
            caret: SelPos {
                entry: entry_id(&chat, 1),
                line: 0,
                col: 6,
            },
        });
        let surface = view.draw(&ctx);
        let grid = crate::test_support::flatten(&surface);
        // Read the text of the highlighted cells directly, so the assertion is
        // about the covered text and not the scrollbar thumb in the last column.
        let covered: String = grid[2][0..6].iter().map(|c| c.char.grapheme()).collect();
        assert_eq!(covered, " row 1", "text under the highlight is preserved");

        // The covered cells carry the selection background.
        for c in 0..6 {
            assert_eq!(grid[2][c].style.bg, bg, "cell (2,{c}) is highlighted");
        }
        // Cells past the caret on that row are untouched.
        for c in 6..40 {
            assert_ne!(grid[2][c].style.bg, bg, "cell (2,{c}) is not highlighted");
        }
        // No other row is highlighted.
        assert_eq!(highlighted_rows(&grid, bg), vec![2]);
    }

    /// A selection whose entries lie entirely off-screen paints nothing.
    #[test]
    fn offscreen_selection_paints_nothing() {
        let chat = chat_with_notices(20);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let bg = view.styles.selection_bg;

        // Top of the transcript shows entries 0..=4; select entries well below.
        view.follow_tail = false;
        view.list.borrow_mut().scroll_lines(-1000);
        let _ = view.draw(&ctx);
        view.selection = Some(Selection {
            anchor: SelPos {
                entry: entry_id(&chat, 15),
                line: 0,
                col: 0,
            },
            caret: SelPos {
                entry: entry_id(&chat, 16),
                line: 0,
                col: 4,
            },
        });
        let grid = crate::test_support::flatten(&view.draw(&ctx));
        assert!(
            highlighted_rows(&grid, bg).is_empty(),
            "off-screen selection painted on screen",
        );
    }

    /// The same entry-relative selection highlights different screen rows as
    /// the viewport scrolls: the selection tracks content, not the viewport.
    #[test]
    fn highlight_tracks_content_across_scroll() {
        let chat = chat_with_notices(20);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let bg = view.styles.selection_bg;

        // Bottom: the last five entries are visible. Highlight " row 16"
        // (entry 16, its first line), which sits at screen row 2.
        let _ = view.draw(&ctx);
        view.selection = Some(Selection {
            anchor: SelPos {
                entry: entry_id(&chat, 16),
                line: 0,
                col: 0,
            },
            caret: SelPos {
                entry: entry_id(&chat, 16),
                line: 0,
                col: 6,
            },
        });
        let surface = view.draw(&ctx);
        let rows = crate::test_support::rows(&surface);
        let grid = crate::test_support::flatten(&surface);
        assert_eq!(rows[2], " row 16", "entry 16 is screen row 2 at the bottom");
        assert_eq!(highlighted_rows(&grid, bg), vec![2]);

        // Scroll up two lines: the same entry moves down two screen rows.
        view.follow_tail = false;
        view.list.borrow_mut().scroll_lines(-2);
        let surface = view.draw(&ctx);
        let rows = crate::test_support::rows(&surface);
        let grid = crate::test_support::flatten(&surface);
        assert_eq!(rows[4], " row 16", "the same content moved down two rows");
        assert_eq!(highlighted_rows(&grid, bg), vec![4]);
    }

    /// A left press-drag-release over a real range copies the extracted text to
    /// the clipboard via OSC 52 and keeps the range highlighted.
    #[test]
    fn press_drag_release_copies_and_keeps_the_highlight() {
        let chat = chat_with_notices(20);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        view.follow_tail = false;
        view.list.borrow_mut().scroll_lines(-1000);
        let _ = view.draw(&ctx);

        // Press on " row 1" (entry 1, screen row 2 at the top): a fresh anchor,
        // follow-tail off.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(0, 2, mouse::Type::Press));
        assert!(view.selection.is_some(), "press anchors a selection");
        assert!(!view.follow_tail, "press disengages follow-tail");

        // Drag to col 6: the caret follows.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(6, 2, mouse::Type::Drag));
        let sel = view.selection.expect("selection present");
        let id1 = entry_id(&chat, 1);
        assert_eq!(sel.anchor.entry, id1);
        assert_eq!((sel.anchor.line, sel.anchor.col), (0, 0));
        assert_eq!(sel.caret.entry, id1);
        assert_eq!((sel.caret.line, sel.caret.col), (0, 6));

        // Release copies the extracted range via OSC 52 and keeps the
        // highlight.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(6, 2, mouse::Type::Release));
        let copied = ec.cmds.iter().find_map(|cmd| match cmd {
            vaxis::vxfw::Command::CopyToClipboard(text) => Some(text.clone()),
            _ => None,
        });
        assert_eq!(copied.as_deref(), Some(" row 1"), "copied the selection");
        assert!(
            view.selection.is_some(),
            "a real range stays highlighted after copy",
        );
    }

    /// A drag past the bottom edge auto-scrolls the list so the selection can
    /// span more than one screen.
    #[test]
    fn drag_past_the_bottom_edge_autoscrolls() {
        let chat = chat_with_notices(20);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        // Start scrolled up so there is room to scroll down.
        view.follow_tail = false;
        view.list.borrow_mut().scroll_lines(-1000);
        let _ = view.draw(&ctx);
        let before = view.list.borrow().scroll_top();

        // Press inside the viewport, then drag below its bottom row.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(2, 5, mouse::Type::Press));
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(2, 20, mouse::Type::Drag));
        let _ = view.draw(&ctx);
        assert!(
            view.list.borrow().scroll_top() > before,
            "dragging past the bottom scrolled the viewport down",
        );
    }

    /// Esc clears a live selection (and is consumed), and a plain click (press
    /// then release at the same point, no drag) clears it too.
    #[test]
    fn esc_and_plain_click_clear_the_selection() {
        let chat = chat_with_notices(20);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        view.follow_tail = false;
        view.list.borrow_mut().scroll_lines(-1000);
        let _ = view.draw(&ctx);

        let a_selection = || Selection {
            anchor: SelPos {
                entry: entry_id(&chat, 1),
                line: 0,
                col: 0,
            },
            caret: SelPos {
                entry: entry_id(&chat, 1),
                line: 0,
                col: 6,
            },
        };

        // Esc clears.
        view.selection = Some(a_selection());
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &key_press(Key::ESCAPE, Modifiers::empty()));
        assert!(view.selection.is_none(), "Esc cleared the selection");
        assert!(ec.consume_event, "Esc was consumed");

        // A plain click clears: a press re-anchors a zero-width selection, and
        // the release with no drag drops it.
        view.selection = Some(a_selection());
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(4, 5, mouse::Type::Press));
        let sel = view.selection.expect("press re-anchored");
        assert!(sel.anchor == sel.caret, "a press is a zero-width selection");
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(4, 5, mouse::Type::Release));
        assert!(
            view.selection.is_none(),
            "a plain click cleared the selection"
        );
    }
}
