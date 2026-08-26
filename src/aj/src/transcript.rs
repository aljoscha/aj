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
use std::rc::{Rc, Weak};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use aj_agent::events::AgentId;
use aj_agent::message::TaskOutcome;
use aj_agent::tool::{
    BashStreamTruncation, TaskStatus, TodoPriority, TodoStatus, ToolDetails, TruncationCause,
};
use aj_app::chat::{
    AssistantEntry, ChatState, CompactionEntry, Entry, EntryId, EntryKind, NoticeLevel,
    SubAgentEntry, SubAgentStatus, TaskNotificationEntry, ToolEntry, ToolStatus, UserEntry,
};
use aj_app::footer::format_tokens;
use aj_app::keybindings::{
    ACTION_BRANCH_MESSAGE, ACTION_COPY_MESSAGE, ACTION_THINKING_TOGGLE, action_shortcut,
    format_keybinding,
};
use aj_app::markdown::{Emphasis, RenderOpts};
use aj_app::theme::{ColorMode, Theme, ThemeBg, ThemeColor, ThemeRgb, rgb_to_256};
use aj_models::types::AssistantContent;
use aj_tools::sanitize_terminal_output;
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;
use vaxis::cell::{Cell, Character, Color, Style};
use vaxis::gwidth;
use vaxis::key::{Key, Modifiers};
use vaxis::mouse;
use vaxis::vxfw::{
    Builder, DrawContext, Event, EventContext, ListView, MaxSize, PadValues, Padding,
    RelativePoint, RichText, ScrollBars, Size, Source, SubSurface, Surface, TextSpan, Widget,
    WidgetRef,
};

use crate::bubble::{Bubble, BubbleBorder, PADDING_X};
use crate::image_store::{ImageRender, ImageStore};
use crate::markdown_view::{MarkdownSegment, MarkdownStyles, MarkdownView};
use crate::selection_copied::SelectionCopied;
use crate::subagent_box::{SubAgentBox, build_subagent_box, surface_rows};
use crate::terminal::TerminalCaps;
use crate::tool_cell::{
    EXPAND_KEY_LABEL, HintKind, build_tool_cell, expand_hint, strikethrough_spans,
};

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
    /// Gray tint for the chat scrollbar thumb. Concrete app scrollbar chrome, so
    /// it stays a concrete gray rather than the faint attribute `dim` carries.
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
    /// Background tint for a transcript selection's highlight, the theme's
    /// `TextSelectionBg` token (see Spec E section 2): a macOS-style blue on
    /// light themes and a darker blue on dark themes, distinct from the
    /// menu-cursor band. Only the background is restyled over the composed
    /// frame, so the selected text stays readable.
    pub(crate) selection_bg: Color,
    /// Foreground mapper for markdown span roles, consumed by
    /// [`MarkdownView`]. Rebuilt from the theme here so a runtime swap
    /// re-tints markdown through the same `set_styles` path.
    pub(crate) markdown: MarkdownStyles,
    /// Whether markdown links emit OSC-8 hyperlinks, from
    /// [`TerminalCaps`](crate::terminal::TerminalCaps).
    pub(crate) hyperlinks: bool,
    /// Whether tool-result images render inline via kitty graphics, from
    /// [`TerminalCaps`](crate::terminal::TerminalCaps). False keeps the text
    /// placeholder.
    pub(crate) images: bool,
}

/// The SGR-2 faint attribute over the default foreground, used by every dim
/// transcript row, tool-cell detail, and background-task line. It is an
/// attribute, not a palette gray, so it tracks the terminal's own foreground.
pub(crate) fn faint() -> Style {
    Style {
        dim: true,
        ..Style::default()
    }
}

impl TranscriptStyles {
    pub(crate) fn from_theme(theme: &Theme, caps: TerminalCaps) -> TranscriptStyles {
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
            scrollbar_thumb: fg(ThemeColor::Muted),
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
            selection_bg: bg(ThemeBg::TextSelectionBg),
            markdown: MarkdownStyles::from_theme(theme),
            hyperlinks: caps.hyperlinks,
            images: caps.images,
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

/// Source-line count shown for a collapsed task notification while
/// folded. A short preview keeps a long notification from flooding the
/// scrollback while still surfacing its first (and most informative)
/// line.
const NOTIFICATION_COLLAPSED_LINES: usize = 10;

/// Upper bound on cached slots. The live working set is the viewport (a
/// screen's worth of entries), so this sits comfortably above it. Overflow
/// evicts the least-recently-used slot, which is correctness-neutral: a
/// dropped slot simply rebuilds on its next draw. Bounding the map keeps a
/// long append-only session from growing it without limit.
const ENTRY_CACHE_CAPACITY: usize = 512;

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
/// `tools_expanded`, `show_thinking_block`, `show_token_usage`,
/// `compact_transcript`, `syntax_highlight`, `show_image_in_terminal`, the
/// active view) are handled by clearing the whole cache when they change rather
/// than folding them into every fingerprint (see [`TranscriptView::draw`] and
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
    /// The model incarnation the slots were built under. Entry ids restart
    /// whenever the model does, so slots of an older one are not stale, they
    /// belong to different entries that happen to share a key.
    generation: u64,
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
            generation: 0,
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

    /// Empty the cache when the model it was filled for is gone.
    ///
    /// The one invalidation no fingerprint can express. A fingerprint says
    /// whether an entry changed, and after a reset there is no old entry to
    /// have changed: `EntryId(0)` of the new incarnation is a different entry
    /// that happens to be filed where the old one was, and the fingerprint is
    /// a length proxy, so two short prompts agree. Checked here rather than at
    /// the callers because a caller that forgets shows the previous session's
    /// content, and the ones that used to remember could not cover the paths
    /// that do not draw.
    fn retire(&mut self, generation: u64) {
        if self.generation != generation {
            self.slots.clear();
            self.generation = generation;
        }
    }

    /// The cached surface for `key` when the slot's fingerprint and width both
    /// match (a HIT). Otherwise `None` (a MISS), and the caller rebuilds and
    /// calls [`insert`](Self::insert).
    ///
    /// `generation` is the model incarnation `key` belongs to, and a lookup
    /// under a new one empties the cache first (see [`Self::retire`]).
    fn get(
        &mut self,
        key: (AgentId, EntryId),
        generation: u64,
        fingerprint: u64,
        width: u16,
    ) -> Option<Surface> {
        self.retire(generation);
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
    fn insert(
        &mut self,
        key: (AgentId, EntryId),
        generation: u64,
        fingerprint: u64,
        width: u16,
        surface: Surface,
    ) {
        self.retire(generation);
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
    /// The model incarnation the slots were built under, for the same reason
    /// [`EntryRenderCache::generation`] carries one, and more sharply: this key
    /// is an entry id alone.
    generation: u64,
    /// Monotonic lookup counter stamped onto a slot's `last_used` on every
    /// access, so eviction can drop the coldest slot.
    tick: u64,
}

impl EntryTextCache {
    fn new() -> EntryTextCache {
        EntryTextCache {
            slots: HashMap::new(),
            generation: 0,
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

    /// Empty the cache when the model it was filled for is gone, the same
    /// invalidation [`EntryRenderCache::retire`] performs and for the same
    /// reason. Select-to-copy reaches this cache without drawing, so a clear
    /// hung off the draw path would not cover it.
    fn retire(&mut self, generation: u64) {
        if self.generation != generation {
            self.slots.clear();
            self.generation = generation;
        }
    }

    /// The cached rows for `id` when the slot's fingerprint and width both
    /// match, else `None` (the caller then lays the entry out and inserts).
    ///
    /// `generation` is the model incarnation `id` belongs to, and a lookup
    /// under a new one empties the cache first (see [`Self::retire`]).
    fn get(
        &mut self,
        id: EntryId,
        generation: u64,
        fingerprint: u64,
        width: u16,
    ) -> Option<Rc<Vec<Vec<Cell>>>> {
        self.retire(generation);
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
    fn insert(
        &mut self,
        id: EntryId,
        generation: u64,
        fingerprint: u64,
        width: u16,
        rows: Rc<Vec<Vec<Cell>>>,
    ) {
        self.retire(generation);
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
/// Which border chrome a user bubble carries, if any. Folded into the
/// per-entry render fingerprint so the cache re-renders on a border change.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum EntryBorder {
    None,
    /// The transcript-focus highlight, with the copy / branch key hint.
    Focus,
    /// The armed-branch highlight, with the branching / cancel hint.
    Branch,
}

struct EntryBuilder {
    chat: Rc<RefCell<ChatState>>,
    styles: Rc<TranscriptStyles>,
    cache: Rc<RefCell<EntryRenderCache>>,
    /// The transcript-focus flag, shared with [`TranscriptView`] and the
    /// keymap host context. Read live so the focus border tracks the current
    /// mode. The transcript is the single writer.
    focus_mode: Rc<std::cell::Cell<bool>>,
    /// The armed-branch message id, shared with the Shell (its single writer).
    /// Read live so the branch border stays on the branched-from message while
    /// the editor, not the transcript, holds focus. `Some` iff a branch is
    /// armed.
    branch_armed: Rc<RefCell<Option<String>>>,
    /// The pre-styled focus hint (`y to copy \u{b7} b to branch`) and the
    /// branch hint (`branching \u{b7} Esc to cancel`), resolved once through the
    /// keybinding data. Shared by `Rc` so each `CachingEntry` clones a handle
    /// rather than the spans.
    copy_label: Rc<Vec<TextSpan>>,
    branch_label: Rc<Vec<TextSpan>>,
    /// The per-session image store, shared with [`TranscriptView`] and the
    /// host loop. `item_at_idx` reads it for a tool-result image's transmitted
    /// id and records visible-but-untransmitted images as pending; the host
    /// drains that pending set after the frame to transmit them.
    image_store: Rc<RefCell<ImageStore>>,
}

impl EntryBuilder {
    /// The border an entry carries this frame. The armed-branch highlight wins
    /// over the focus highlight: arming moves focus off the transcript, but the
    /// box must stay on the branched-from message. Only user bubbles border.
    fn border_for(&self, entry: &Entry, idx: usize, cursor: usize) -> EntryBorder {
        let EntryKind::User(user) = &entry.kind else {
            return EntryBorder::None;
        };
        if user.message_id.is_some() && *self.branch_armed.borrow() == user.message_id {
            EntryBorder::Branch
        } else if self.focus_mode.get() && idx == cursor {
            EntryBorder::Focus
        } else {
            EntryBorder::None
        }
    }

    /// How a tool-result image `entry` should render this frame.
    ///
    /// `images_enabled` is `styles.images && chat.show_image_in_terminal`,
    /// computed by the caller (which already holds the `chat` borrow, so we do
    /// not re-borrow here). Returns [`ImageRender::Disabled`] when images are
    /// off or `entry` is not a tool-result image, which draws the text
    /// fallback. Otherwise returns [`ImageRender::Transmitted`] when the store
    /// has an id, [`ImageRender::Failed`] when a prior transmit gave up (the
    /// text fallback again, never retried), or records the entry as pending and
    /// returns [`ImageRender::Pending`] (the blank reserve for one frame).
    /// Recording
    /// the key on a visible-but-untransmitted image is what makes transmission
    /// lazy: only entries drawn this frame get recorded, and the host transmits
    /// them after the frame.
    fn resolve_image(&self, agent: AgentId, entry: &Entry, images_enabled: bool) -> ImageRender {
        if !images_enabled {
            return ImageRender::Disabled;
        }
        let EntryKind::Tool(tool) = &entry.kind else {
            return ImageRender::Disabled;
        };
        if !matches!(tool.details, Some(ToolDetails::Image { .. })) {
            return ImageRender::Disabled;
        }
        // Bind the reads before the mutable borrow: holding the shared
        // `borrow()` across the `borrow_mut()` below would panic.
        let resolved = {
            let store = self.image_store.borrow();
            if let Some(id) = store.get(agent, entry.id) {
                Some(ImageRender::Transmitted(id))
            } else if store.is_failed(agent, entry.id) {
                Some(ImageRender::Failed)
            } else {
                None
            }
        };
        match resolved {
            Some(render) => render,
            None => {
                self.image_store
                    .borrow_mut()
                    .record_pending(agent, entry.id);
                ImageRender::Pending
            }
        }
    }
}

impl Builder for EntryBuilder {
    fn item_at_idx(&self, idx: usize, cursor: usize) -> Option<WidgetRef> {
        let chat = self.chat.borrow();
        let agent = chat.active_view();
        let entry = chat.transcript(agent)?.entries().get(idx)?;
        // The bubble border is per-cursor / per-branch chrome, not entry
        // content, so fold it into the fingerprint. Without this the cache
        // would replay a stale bordered or unbordered surface when focus or the
        // armed branch moves. Folding it in re-renders exactly the entry
        // gaining and the entry losing the border and leaves the rest cache
        // hits.
        let border = self.border_for(entry, idx, cursor);
        // Resolve how this entry's tool-result image renders. Images are on
        // only when the terminal supports them (`styles.images`, the caps
        // probe) and the user has not turned them off
        // (`show_image_in_terminal`). Reading the config here, under the borrow
        // the caller already holds, keeps the gate on one seam. Recording a
        // visible-but-untransmitted image as pending is what makes transmission
        // lazy: only entries drawn this frame get recorded.
        let images_enabled = self.styles.images && chat.show_image_in_terminal;
        let image = self.resolve_image(agent, entry, images_enabled);
        let mut hasher = DefaultHasher::new();
        fingerprint_into(entry, &chat, &mut hasher);
        border.hash(&mut hasher);
        // Fold the image render state so the entry rebuilds the frame its
        // transmit resolves: `Pending` -> `Transmitted` places the image and
        // `Pending` -> `Failed` swaps the blank reserve for text. `Disabled`
        // and `Pending` fold identically. The config toggle between them rides
        // the wholesale `GlobalRenderInputs` clear instead.
        image.render_tag().hash(&mut hasher);
        let fingerprint = hasher.finish();
        // A Running sub-agent box animates its glyph off the wall-clock, so it
        // must rebuild on every redraw, not just when its fingerprint changes.
        // We bypass the cache for it rather than fold the clock into the
        // fingerprint. The rebuild is cheap (a title and one activity line, no
        // markdown), and the redraw pump ticks while any sub-agent runs.
        let bypass_cache =
            matches!(&entry.kind, EntryKind::SubAgent(s) if s.status == SubAgentStatus::Running);
        Some(Rc::new(RefCell::new(CachingEntry {
            cache: Rc::clone(&self.cache),
            chat: Rc::clone(&self.chat),
            styles: Rc::clone(&self.styles),
            agent,
            entry_id: entry.id,
            fingerprint,
            generation: chat.generation(),
            border,
            bypass_cache,
            copy_label: Rc::clone(&self.copy_label),
            branch_label: Rc::clone(&self.branch_label),
            image,
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
    /// The model incarnation `entry_id` belongs to. Read under the same borrow
    /// as the fingerprint, so the pair always describes one observation.
    generation: u64,
    /// Which border this entry's bubble gets on the miss-path build. Already
    /// folded into `fingerprint`.
    border: EntryBorder,
    /// Whether to skip the render cache and rebuild on every draw. Set for a
    /// `Running` sub-agent box, whose spinner glyph advances on the wall-clock
    /// and so cannot be served from a fingerprint-keyed surface.
    bypass_cache: bool,
    /// The hint spans shown on the border's bottom edge: `copy_label` for a
    /// focus border, `branch_label` for a branch border. Both are held so the
    /// miss-path build picks by `border` without another lookup.
    copy_label: Rc<Vec<TextSpan>>,
    branch_label: Rc<Vec<TextSpan>>,
    /// How this entry's tool-result image renders this frame. Already folded
    /// into `fingerprint` via its transmitted id, so the cached surface flips
    /// when the id arrives. `Disabled` (images off or non-image) and `Pending`
    /// carry no id, so the config toggle between them relies on the wholesale
    /// `GlobalRenderInputs` clear, not this fingerprint.
    image: ImageRender,
}

impl Widget for CachingEntry {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let width = ctx.max.width.unwrap_or(ctx.min.width);
        let key = (self.agent, self.entry_id);

        // HIT: the stored surface was drawn for this fingerprint and width, so
        // replay it verbatim. Bind the lookup to a `let` so the cache's
        // `RefMut` is released before the miss path re-borrows it. A
        // bypass entry (an animated Running box) always rebuilds.
        if !self.bypass_cache {
            let cached = self
                .cache
                .borrow_mut()
                .get(key, self.generation, self.fingerprint, width);
            if let Some(surface) = cached {
                return surface;
            }
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
            // The border is threaded only for a focused or branch-armed user
            // message, with its matching hint on the bottom edge. Every other
            // entry builds unbordered.
            let label = match self.border {
                EntryBorder::None => None,
                EntryBorder::Focus => Some(self.copy_label.as_slice()),
                EntryBorder::Branch => Some(self.branch_label.as_slice()),
            };
            let mut widget =
                build_entry_widget(entry, &chat, &self.styles, false, label, self.image)
                    .into_indented_boxed();
            widget.draw(ctx)
        };
        // A bypass entry is never stored, so it can't strand a stale slot when
        // its glyph advances or when it later concludes and becomes cacheable.
        if !self.bypass_cache {
            self.cache.borrow_mut().insert(
                key,
                self.generation,
                self.fingerprint,
                width,
                surface.clone(),
            );
        }
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
/// `show_thinking_block`, `show_token_usage`, `compact_transcript`,
/// `syntax_highlight`, `show_image_in_terminal`, the active view, the theme,
/// the draw width) are NOT hashed here: the cache clears wholesale when they
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
        }
        EntryKind::SubAgent(s) => {
            3u8.hash(hasher);
            subagent_fingerprint(s, hasher);
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
        EntryKind::TaskNotification(n) => {
            7u8.hash(hasher);
            n.body.len().hash(hasher);
            // The outcome drives the bubble tint, so fold in a
            // per-outcome discriminant.
            task_outcome_tag(&n.outcome).hash(hasher);
        }
    }
}

/// Assistant / reasoning fields: the content-block count, a per-block tag (so
/// a block changing kind is caught), the summed text and thinking byte
/// lengths, the thinking `redacted` flag (which flips the placeholder text
/// without changing its length), and `finalized`. `show_thinking_block` is a
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

/// The details variant discriminant plus enough payload content to invalidate
/// every rendered change. Streaming text uses length proxies because it only
/// grows. Immutable canonical diffs use their precomputed content fingerprint.
fn details_fingerprint(details: &ToolDetails, hasher: &mut DefaultHasher) {
    match details {
        ToolDetails::Text { summary, body } => {
            0u8.hash(hasher);
            summary.len().hash(hasher);
            body.len().hash(hasher);
        }
        ToolDetails::Diff(diff) => {
            1u8.hash(hasher);
            diff.content_fingerprint().hash(hasher);
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

/// Sub-agent box fields, hashed by full value. The box renders from this
/// metadata, not the child transcript, so the fingerprint keys on the exact
/// inputs a concluded box reads: the status tag, the task, the report, the
/// latest-activity line, and the background flag.
///
/// The report and activity strings are hashed by full value, not a length
/// proxy, because a same-length activity transition (for example `bash` to
/// `grep`) must still invalidate the cache, otherwise a stale line survives.
///
/// A `Running` box bypasses the render cache entirely (its glyph animates on
/// the wall-clock, see `CachingEntry`), so this fingerprint only actually
/// gates a concluded box's cached surface.
fn subagent_fingerprint(s: &SubAgentEntry, hasher: &mut DefaultHasher) {
    match s.status {
        SubAgentStatus::Running => 0u8.hash(hasher),
        SubAgentStatus::Done => 1u8.hash(hasher),
        SubAgentStatus::Truncated => 2u8.hash(hasher),
        SubAgentStatus::Failed => 3u8.hash(hasher),
    }
    s.task.hash(hasher);
    s.report.hash(hasher);
    s.latest_activity.hash(hasher);
    s.background.hash(hasher);
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
        Some(TaskStatus::CaptureFailed(code)) => {
            4u8.hash(hasher);
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
    /// Erase to a boxed widget, applying the shared one-column left indent
    /// every transcript entry carries.
    ///
    /// The tinted bubble and sub-agent box entries already inset their content
    /// by `PADDING_X` over a full-width background, so they are boxed as-is.
    /// The background-less rich and markdown entries paint from column zero, so
    /// we wrap them in a `Padding` that shifts their content right by
    /// `PADDING_X`, leaving a true blank first column (no background) which
    /// semantic selection skips.
    ///
    /// NOTE: an untinted or too-narrow bubble falls back to a flush plain
    /// render at column zero, so it is not indented here. That path is not a
    /// live top-level entry (a top-level tool bubble is always tinted and
    /// rendered wide), so it does not break the transcript's alignment today.
    pub(crate) fn into_indented_boxed(self) -> Box<dyn Widget> {
        match self {
            EntryWidget::Bubble(b) => Box::new(b),
            EntryWidget::SubAgent(b) => Box::new(b),
            EntryWidget::Rich(r) => Box::new(indent_entry(r)),
            EntryWidget::Markdown(m) => Box::new(indent_entry(m)),
        }
    }
}

/// Wrap a background-less entry widget in the shared left indent, matching the
/// column-one text start of the bubble entries.
fn indent_entry<W: Widget + 'static>(widget: W) -> Padding {
    Padding {
        child: Rc::new(RefCell::new(widget)),
        padding: PadValues {
            left: PADDING_X,
            ..PadValues::default()
        },
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
///
/// `image` is how a tool-result image entry renders this frame, resolved from
/// the capability-and-config gate and the shared image store by the caller.
/// Only the `Tool` arm reads it.
pub(crate) fn build_entry_widget(
    entry: &Entry,
    chat: &ChatState,
    styles: &TranscriptStyles,
    nested: bool,
    focus: Option<&[TextSpan]>,
    image: ImageRender,
) -> EntryWidget {
    match &entry.kind {
        EntryKind::Tool(tool) => EntryWidget::Bubble(build_tool_cell(
            tool,
            chat.tasks(),
            chat.tools_expanded,
            chat.compact_transcript,
            styles,
            image,
        )),
        EntryKind::User(user) => EntryWidget::Bubble(build_user_bubble(user, styles, focus)),
        EntryKind::TaskNotification(n) => {
            EntryWidget::Bubble(build_task_notification(n, chat.tools_expanded, styles))
        }
        EntryKind::SubAgent(s) if !nested => EntryWidget::SubAgent(build_subagent_box(
            s,
            chat.tools_expanded,
            chat.syntax_highlight,
            styles,
        )),
        // Assistant prose and the expanded compaction summary render as
        // markdown through the width-aware `MarkdownView`. A nested assistant
        // entry (inside a sub-agent box) takes this path too, so a child's
        // messages render as markdown just like the top-level ones.
        EntryKind::Assistant(a) => EntryWidget::Markdown(build_assistant_markdown(
            a,
            chat.show_thinking_block,
            chat.syntax_highlight,
            styles,
        )),
        EntryKind::Compaction(c) => EntryWidget::Markdown(build_compaction_markdown(
            c,
            chat.tools_expanded,
            chat.syntax_highlight,
            styles,
        )),
        // Token-usage rows are hidden when the toggle is off. They render as
        // an empty (zero-height) rich text rather than being dropped from the
        // list, so the entry index stays aligned with selection and focus,
        // which key on it.
        EntryKind::TurnUsage(_) if !chat.show_token_usage => {
            EntryWidget::Rich(RichText::new(Vec::new()))
        }
        _ => EntryWidget::Rich(RichText::new(entry_spans(entry, styles))),
    }
}

/// Build the user-message bubble: the full message under the
/// user-message tint, with no `> ` prefix (the tint is the entire
/// visual cue, which also keeps the text cleanly copy-pasteable).
///
/// When `focus` is `Some`, the bubble carries the focus border in the
/// `borderAccent` color, with the supplied copy-key hint on its bottom edge
/// (Spec E section 2).
fn build_user_bubble(
    user: &UserEntry,
    styles: &TranscriptStyles,
    focus: Option<&[TextSpan]>,
) -> Bubble {
    let span = |text: String, style: Style| TextSpan {
        text,
        style,
        ..TextSpan::default()
    };
    let text = user.joined_text();
    let mut spans = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            spans.push(span("\n".into(), styles.user));
        }
        spans.push(span(line.to_string(), styles.user));
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

/// Build a task-notification bubble: the notice body under an
/// outcome-tinted surface, styled like a tool cell rather than a user
/// prompt.
///
/// The tint reflects the outcome (success vs did-not-succeed), matching
/// how a tool cell tints by status. A long notice folds to its first
/// [`NOTIFICATION_COLLAPSED_LINES`] source lines plus a dim expand hint,
/// expanding together with tool output under the session-wide
/// `tools_expanded` flag. The body embeds captured task output, so it
/// runs through [`sanitize_terminal_output`].
fn build_task_notification(
    notification: &TaskNotificationEntry,
    expanded: bool,
    styles: &TranscriptStyles,
) -> Bubble {
    let span = |text: String, style: Style| TextSpan {
        text,
        style,
        ..TextSpan::default()
    };
    let text = sanitize_terminal_output(&notification.body);
    let mut lines: Vec<&str> = text.lines().collect();
    let fold = !expanded && lines.len() > NOTIFICATION_COLLAPSED_LINES;
    let hint = fold.then(|| {
        let more = lines.len() - NOTIFICATION_COLLAPSED_LINES;
        lines.truncate(NOTIFICATION_COLLAPSED_LINES);
        expand_hint(more, HintKind::More)
    });
    let mut spans = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            spans.push(span("\n".into(), styles.text));
        }
        spans.push(span((*line).to_string(), styles.text));
    }
    if let Some(hint) = hint {
        spans.push(span("\n".into(), styles.text));
        spans.push(span(hint, styles.dim));
    }
    let bg = task_notification_bg(&notification.outcome, styles);
    Bubble::entry(spans, Some(bg), styles.text)
}

/// The bubble tint for a task notification's outcome: success reads as
/// a done tool cell, and every did-not-succeed outcome (failed or
/// killed) reads as an errored one.
fn task_notification_bg(outcome: &TaskOutcome, styles: &TranscriptStyles) -> Color {
    match outcome {
        TaskOutcome::Succeeded => styles.tool_success_bg,
        TaskOutcome::Failed { .. } | TaskOutcome::Killed => styles.tool_error_bg,
    }
}

/// Stable per-outcome discriminant folded into an entry's fingerprint,
/// since the outcome drives the bubble tint.
fn task_outcome_tag(outcome: &TaskOutcome) -> u8 {
    match outcome {
        TaskOutcome::Succeeded => 0,
        TaskOutcome::Failed { .. } => 1,
        TaskOutcome::Killed => 2,
    }
}

/// The pre-styled shortcut hint shown on the focused-message border's bottom
/// edge, resolved through the keybinding data so the keys are never literals
/// (Spec E section 2). Each key renders in the accent color and the rest
/// muted, the way an overlay styles the key hints in its chrome. Both the copy
/// and branch shortcuts share the one line (`y to copy · b to branch`).
fn copy_label_spans(styles: &TranscriptStyles) -> Vec<TextSpan> {
    let copy_key = action_shortcut(ACTION_COPY_MESSAGE).unwrap_or_default();
    let branch_key = action_shortcut(ACTION_BRANCH_MESSAGE).unwrap_or_default();
    let key_span = |text: String| TextSpan {
        text,
        style: styles.accent,
        ..TextSpan::default()
    };
    let dim_span = |text: &str| TextSpan {
        text: text.into(),
        style: styles.dim,
        ..TextSpan::default()
    };
    vec![
        key_span(copy_key),
        dim_span(" to copy"),
        dim_span(" \u{b7} "),
        key_span(branch_key),
        dim_span(" to branch"),
    ]
}

/// The hint shown on an armed-branch bubble's border, mirroring the focus
/// hint's styling: the status word and the key in accent, the rest muted.
/// Cancelling a branch is the bare Esc key (no bound action), so we spell its
/// label through `format_keybinding` rather than a raw literal, keeping it
/// coherent with every other Esc hint.
fn branch_label_spans(styles: &TranscriptStyles) -> Vec<TextSpan> {
    let accent_span = |text: &str| TextSpan {
        text: text.into(),
        style: styles.accent,
        ..TextSpan::default()
    };
    let dim_span = |text: &str| TextSpan {
        text: text.into(),
        style: styles.dim,
        ..TextSpan::default()
    };
    vec![
        accent_span("branching"),
        dim_span(" \u{b7} "),
        accent_span(&format_keybinding("escape")),
        dim_span(" to cancel"),
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
        | EntryKind::TaskNotification(_)
        | EntryKind::Compaction(_) => Vec::new(),
        // The `SubAgent` arm is only reachable as the nested-inside-a-box
        // fallback, which can't occur live (sub-agents don't spawn
        // sub-agents), so a dim stub is enough.
        EntryKind::SubAgent(s) => vec![span(format!("[sub-agent {}]", s.child), styles.dim)],
        // Notice and usage rows render as plain rich text. The shared
        // one-column left indent is applied when the entry is boxed (see
        // `into_indented_boxed`), so it covers wrapped continuation lines too
        // and we add no leading space here.
        EntryKind::Notice(n) => {
            let style = match n.level {
                NoticeLevel::Info => styles.dim,
                NoticeLevel::Warning => styles.warning,
                NoticeLevel::Error => styles.error,
            };
            // Parse any SGR strikethrough markers (the context notice strikes a
            // disabled skill's row) into struck spans. Text with no markers
            // yields a single span, so this is safe for every notice.
            strikethrough_spans(&n.text, style)
        }
        EntryKind::TurnUsage(u) => vec![span(u.line(), styles.dim)],
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

/// Display label of the thinking-toggle chord shown on collapsed thinking
/// blocks, resolved from the shared binding data, so it reflects a user
/// `[keybindings]` override. This is the `aj.thinking.toggle` chord (default
/// `alt+t`), distinct from the tools-expand chord in
/// [`crate::tool_cell::EXPAND_KEY_LABEL`].
static THINKING_EXPAND_KEY_LABEL: LazyLock<String> = LazyLock::new(|| {
    action_shortcut(ACTION_THINKING_TOGGLE).expect("aj.thinking.toggle has a default chord")
});

/// Build the [`MarkdownView`] for an assistant entry: one segment per content
/// block, in order.
///
/// Plain text renders under the normal text style; thinking blocks under the
/// thinking style (its own color plus italic). Tool calls render as their own
/// `Tool` transcript entries, so the inline block is skipped here to avoid
/// duplicating them. Redacted and (when `show_thinking` is off) hidden thinking
/// collapse to their placeholders, matching the plain-text renderer they
/// replace.
fn build_assistant_markdown(
    a: &AssistantEntry,
    show_thinking: bool,
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
            // A collapsed thinking block carries the expand-key hint only when
            // it has a body worth revealing. A body-less block (signed-but-empty
            // thinking, or a provider that omits the transcript) collapses to a
            // bare placeholder so we do not advertise a toggle that reveals
            // nothing.
            AssistantContent::Thinking(t) if !show_thinking => {
                let text = if t.thinking.is_empty() {
                    "Thinking…".to_string()
                } else {
                    format!("Thinking… ({} to expand)", *THINKING_EXPAND_KEY_LABEL)
                };
                MarkdownSegment {
                    text,
                    opts: thinking_opts.clone(),
                    base_style: styles.thinking,
                }
            }
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
/// The header stays a plain (non-markdown) leading row so its token glyphs
/// survive verbatim, carrying the `(<key> to expand)` hint while collapsed.
/// Folding rides the session-wide `tools_expanded` flag, the same one tool
/// bodies honor, so a summary expands and collapses together with tool results
/// under one keystroke. The summary renders as a markdown segment only once
/// expanded and non-empty. The shared one-column left indent is applied when
/// the entry is boxed (see `into_indented_boxed`).
fn build_compaction_markdown(
    c: &CompactionEntry,
    tools_expanded: bool,
    syntax_highlight: bool,
    styles: &TranscriptStyles,
) -> MarkdownView {
    let mut header = compaction_header(c.tokens_before, c.tokens_after);
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectionUnit {
    Character,
    Word,
    Line,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WordClass {
    Whitespace,
    Delimiter,
    Regular,
}

#[derive(Clone, Copy)]
struct LastClick {
    at: Instant,
    pos: SelPos,
    count: u8,
}

/// The `(entry, line)` a screen row displays, produced by the per-frame walk
/// over realized entries. Used by the highlight to decide, per visible row,
/// which cells the selection covers.
#[derive(Clone, Copy)]
struct RowPos {
    entry: EntryId,
    line: usize,
}

/// Where the viewport should come to rest relative to the focused message
/// once a focus-navigation scroll finishes. Recorded when the scroll starts
/// so the final frame can land on the exact position the snap computes,
/// correcting any drift the estimate-based travel accrued.
///
/// `Top` and `Bottom` keep [`FOCUS_SCROLL_MARGIN`] rows of context on the side
/// we scrolled from, so a stepped-to message never sits flush against a
/// viewport edge.
#[derive(Clone, Copy)]
enum FocusAnchor {
    /// The message's top sits `margin` rows below the viewport top (stepping
    /// to an older message, or hugging the top). `margin` is 0 for a message
    /// taller than the viewport, which is pinned to the top so its head shows.
    Top { margin: u16 },
    /// The message's bottom sits `margin` rows above the viewport bottom
    /// (stepping to a newer message below the fold, or hugging the bottom).
    Bottom { margin: u16 },
    /// The message is already framed with context on both sides. The snap only
    /// keeps it in view via `ensure_scroll`, it does not reframe.
    Keep,
}

/// How a [`ScrollAnim`] lands on its final frame.
#[derive(Clone, Copy)]
enum ScrollCompletion {
    /// Snap onto the focused message at the recorded anchor. Focus travel is
    /// estimate-based, so the snap corrects any residual drift.
    Focus(FocusAnchor),
    /// Apply the exact remaining line delta and stop. Page travel is an exact
    /// line count, and the glide also stops early once the viewport reaches the
    /// end it is heading for (see [`at_scroll_end`](TranscriptView::at_scroll_end)).
    Page,
}

/// An in-flight smooth scroll of the transcript viewport.
///
/// Both focus navigation and page scrolling move the viewport by a time-based
/// eased glide: [`total`](Self::total) lines over [`duration`](Self::duration),
/// driven by a self-scheduled tick chain. How the final frame lands depends on
/// [`completion`](Self::completion).
struct ScrollAnim {
    /// Signed line distance to travel: negative scrolls toward the top.
    total: f64,
    /// Lines already fed to the list this animation, so each tick applies only
    /// the incremental delta and per-frame rounding cannot drift.
    applied: f64,
    completion: ScrollCompletion,
    start: Instant,
    duration: Duration,
}

/// Interval between smooth-scroll frames, matching the drive loop's 60 fps cap.
const SCROLL_ANIM_FRAME_MS: u32 = 16;
/// Below this travel distance a focus step just snaps: a move of a line or two
/// is not worth animating and would only add latency.
const SCROLL_ANIM_MIN_LINES: f64 = 2.0;
/// Rows of context kept between a focus-stepped message and the viewport edge
/// it was scrolled toward, so the message never lands flush against the top or
/// bottom. Clamped away at the transcript ends, where there is nothing to show.
const FOCUS_SCROLL_MARGIN: u16 = 3;
/// Terminal mouse reports do not carry the desktop's configured double-click
/// interval. Five hundred milliseconds matches the common desktop default.
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const WORD_DELIMITERS: &str = "./\\()\"'-:,.;<>~!@#$%^&*|+=[]{}?│";
/// Per-line pacing for the glide, clamped to
/// [`SCROLL_ANIM_MIN_MS`]`..=`[`SCROLL_ANIM_MAX_MS`]. Roughly constant speed
/// for short and medium jumps, capped so a long jump stays snappy (the
/// ease-out then front-loads the distance and decelerates into the target).
const SCROLL_ANIM_MS_PER_LINE: f64 = 7.0;
const SCROLL_ANIM_MIN_MS: f64 = 60.0;
const SCROLL_ANIM_MAX_MS: f64 = 160.0;

/// Duration for a glide of `distance` lines (absolute).
#[allow(clippy::as_conversions)]
fn scroll_anim_duration(distance: f64) -> Duration {
    // `distance` is a line count and the product is clamped to a small
    // millisecond range, so the cast to `u64` never overflows or loses signal.
    let ms = (distance * SCROLL_ANIM_MS_PER_LINE).clamp(SCROLL_ANIM_MIN_MS, SCROLL_ANIM_MAX_MS);
    Duration::from_millis(ms as u64)
}

/// A line offset as `f64` for the smooth-scroll math. Transcript line counts
/// are far below the f64 integer-precision limit, so the cast is exact.
#[allow(clippy::as_conversions)]
fn line_as_f64(line: u64) -> f64 {
    line as f64
}

/// Round a signed line delta to `i32` for `ListView::scroll_lines`. The delta
/// is one frame's share of a bounded travel, so it never overflows.
#[allow(clippy::as_conversions)]
fn round_lines(v: f64) -> i32 {
    v.round() as i32
}

/// The chat area: a follow-tail `ListView` over the active transcript,
/// wrapped in [`ScrollBars`] for the vertical scrollbar thumb (Spec E
/// section 1). The bar reserves the rightmost column and hides its
/// thumb while the transcript fits the viewport.
///
/// The bars stamp both their own surface and the inner list's, so
/// content-area mouse events hit-test to the list and bar-column events
/// hit-test to the bars. This view is always an ancestor of both in the
/// hit list, so it observes those events in the capturing phase (wheel-up
/// disengages follow-tail, an active thumb drag is intercepted). It also
/// forwards mouse events to the bars from its own event handlers, which
/// [`observe_mouse`](Self::observe_mouse) explains stays correct and is not
/// double dispatch.
pub struct TranscriptView {
    chat: Rc<RefCell<ChatState>>,
    /// The chat list, shared with `bars`, which draws it and routes
    /// thumb drag-to-jump into it.
    list: Rc<RefCell<ListView>>,
    bars: Rc<RefCell<ScrollBars<ListView>>>,
    /// Memoized per-entry surfaces, shared into the [`EntryBuilder`]. See
    /// [`EntryRenderCache`]. Owned here so it survives across frames and so a
    /// theme swap or a global-toggle change can clear it.
    cache: Rc<RefCell<EntryRenderCache>>,
    /// The transcript's styles, shared into the [`EntryBuilder`]. Kept here so
    /// the per-entry text layout ([`entry_rows`](Self::entry_rows)) builds
    /// entries with the same styles the visible list does. Replaced by
    /// [`set_styles`](Self::set_styles) on a theme swap.
    styles: Rc<TranscriptStyles>,
    /// The per-session image store, shared with the [`EntryBuilder`] (which
    /// records pending images and reads transmitted ids) and the host loop
    /// (which transmits and frees). Held here so [`set_styles`](Self::set_styles)
    /// can rebuild the builder with the same handle on a theme swap.
    image_store: Rc<RefCell<ImageStore>>,
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
    /// The armed-branch message id, shared by `Rc` with the [`EntryBuilder`]
    /// (so the branch border tracks the armed message) and the Shell (its
    /// single writer). Kept so [`set_styles`](Self::set_styles) can rebuild the
    /// builder on a theme swap without losing the handle.
    branch_armed: Rc<RefCell<Option<String>>>,
    /// Called from the Esc branch of transcript-focus mode to hand focus
    /// back to the editor. `None` until the host wires it in `Shell::new`.
    /// The resulting `FocusOut` clears the focus flag, exiting the mode.
    on_exit_focus: Option<Box<dyn FnMut(&mut EventContext)>>,
    /// Called after a plain click on a sub-agent box. The host routes the ID
    /// through the same observe path as the agent picker.
    on_observe_agent: Option<Box<dyn FnMut(AgentId)>>,
    /// A sub-agent box hit on the current left-button press. Cleared by any
    /// drag, so select-to-copy never switches views on release.
    agent_click: Option<AgentId>,
    /// Sub-agent boxes under each row of the last completed bounded draw.
    /// Mouse dispatch uses the previous frame, so hit testing must use its
    /// geometry rather than re-laying live entries between frames.
    agent_hit_rows: Vec<Option<AgentId>>,
    agent_hit_width: u16,
    /// The active free-form selection, if any (Spec E section 2). Set on a
    /// left-button press-drag over the content and kept highlighted after the
    /// release copies it, until the next plain click or Esc clears it.
    selection: Option<Selection>,
    /// The complete unit selected by the current press. A multi-click drag
    /// keeps this word or rendered line selected while extending by whole units.
    selection_origin: Option<Selection>,
    selection_unit: SelectionUnit,
    /// Recent press metadata used to derive double and triple clicks because
    /// terminal mouse protocols provide neither click counts nor timestamps.
    last_click: Option<LastClick>,
    /// The last select-to-copy record. Written on the release that copies a
    /// real range. The drive loop edge-detects fresh records and raises the
    /// copy toast.
    selection_copied: Rc<std::cell::Cell<Option<SelectionCopied>>>,
    /// Viewport size the last completed [`draw`](Widget::draw) laid out
    /// against. The mouse handlers run between draws with no `DrawContext`, so
    /// they read the geometry back from here to map widget-local coordinates
    /// into entry-relative selection positions. Zero before the first draw.
    last_view: Size,
    /// Weak self-reference, so scroll animations can schedule ticks targeting
    /// this widget to drive the smooth focus and page glides (see
    /// [`ScrollAnim`]). Set by the host via
    /// [`set_widget_ref`](Self::set_widget_ref) once the view is behind an
    /// `Rc`. Empty in unit tests, where ticks are never delivered, so a scroll
    /// there snaps rather than animating.
    me: Weak<RefCell<TranscriptView>>,
    /// The in-flight focus or page glide, if any. `None` when the viewport is
    /// at rest. Cleared to cancel a glide (e.g. when a snapping gesture
    /// supersedes it); see [`scroll_tick_scheduled`](Self::scroll_tick_scheduled).
    scroll_anim: Option<ScrollAnim>,
    /// Whether a scroll-animation tick is already pending. Keeps exactly one
    /// tick chain alive: cancelling [`scroll_anim`](Self::scroll_anim) leaves a
    /// pending tick to fire once and stop, and a glide started before that
    /// orphan fires reuses it rather than arming a second (double-speed) chain.
    scroll_tick_scheduled: bool,
}

/// The session-wide render inputs the transcript cache does not fingerprint
/// per entry. A change to any of them invalidates every cached surface, so
/// [`TranscriptView::draw`] clears the cache wholesale on a change. These
/// toggles are rare, so a full clear costs one all-miss frame.
///
/// `show_image_in_terminal` rides here rather than the per-entry fingerprint
/// because that fingerprint folds only an image's transmitted id, absent for
/// both the disabled (text) and pending (blank reserve) states. Toggling the
/// setting flips an entry between those two id-less states, which the per-entry
/// fingerprint cannot see, so the wholesale clear is what makes images appear
/// and disappear without a stale replay.
#[derive(Clone, Copy, PartialEq, Eq)]
struct GlobalRenderInputs {
    active_view: AgentId,
    tools_expanded: bool,
    show_thinking_block: bool,
    show_token_usage: bool,
    compact_transcript: bool,
    syntax_highlight: bool,
    show_image_in_terminal: bool,
}

impl GlobalRenderInputs {
    fn read(chat: &ChatState) -> GlobalRenderInputs {
        GlobalRenderInputs {
            active_view: chat.active_view(),
            tools_expanded: chat.tools_expanded,
            show_thinking_block: chat.show_thinking_block,
            show_token_usage: chat.show_token_usage,
            compact_transcript: chat.compact_transcript,
            syntax_highlight: chat.syntax_highlight,
            show_image_in_terminal: chat.show_image_in_terminal,
        }
    }
}

/// The first content column of a rendered entry row, past the shared chrome
/// indent every entry carries.
///
/// Selection is semantic: it never grabs the blank left margin the transcript
/// insets each entry by (`PADDING_X` columns). We skip at most that many
/// leading blank columns, so content-level leading whitespace past the margin
/// (a code block's indentation, say) is preserved, and a degenerate entry with
/// no margin keeps its first column. The blank test matches the tinted padding
/// a bubble paints as well as the default blank a plain entry leaves.
fn content_start(row: &[Cell]) -> usize {
    let blanks = row
        .iter()
        .take_while(|cell| cell.char.grapheme().trim().is_empty())
        .count();
    blanks.min(usize::from(PADDING_X))
}

fn content_end(row: &[Cell]) -> usize {
    row.iter()
        .rposition(|cell| !cell.char.grapheme().trim().is_empty())
        .map_or_else(|| content_start(row), |i| i + 1)
}

fn word_class(grapheme: &str) -> WordClass {
    if grapheme.chars().all(char::is_whitespace) {
        WordClass::Whitespace
    } else if grapheme
        .chars()
        .next()
        .is_some_and(|ch| WORD_DELIMITERS.contains(ch))
    {
        WordClass::Delimiter
    } else {
        WordClass::Regular
    }
}

/// Expand a display-column range so neither endpoint splits a wide grapheme.
fn expand_to_graphemes(row: &[Cell], from: usize, to: usize) -> (usize, usize) {
    let mut expanded = (from.min(row.len()), to.min(row.len()));
    let mut col = 0;
    while col < row.len() {
        let next = col
            .saturating_add(usize::from(row[col].char.width.max(1)))
            .min(row.len());
        if col < expanded.1 && next > expanded.0 {
            expanded.0 = expanded.0.min(col);
            expanded.1 = expanded.1.max(next);
        }
        col = next;
    }
    expanded
}

/// Read a cell range once per grapheme, preserving selected whitespace while
/// dropping layout padding beyond the row's last content cell.
fn cell_range_text(row: &[Cell], from: usize, to: usize) -> String {
    let (from, to) = expand_to_graphemes(row, from, to.min(content_end(row)));
    let mut out = String::new();
    let mut col = from;
    while col < to {
        let cell = &row[col];
        out.push_str(cell.char.grapheme());
        col = col.saturating_add(usize::from(cell.char.width.max(1)));
    }
    out
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
        // Semantic selection: never read the chrome indent margin, so start at
        // the content column even when the range reaches into it.
        let from = from.max(content_start(cells));
        let from = from.min(cells.len());
        let to = to.min(cells.len()).max(from);
        if row != start_row {
            out.push('\n');
        }
        out.push_str(&cell_range_text(cells, from, to));
    }
    out
}

impl TranscriptView {
    /// Build the view over `chat`. `focused` is the transcript-focus flag,
    /// shared with the keymap host context so the copy chord and the focus
    /// border read the same state (this view is its single writer).
    /// `branch_armed` is the armed-branch message id, shared with the Shell
    /// (its single writer) so the branch border tracks the armed message.
    /// `image_store` is the per-session image store, shared with the host loop
    /// so the builder records pending images the host then transmits.
    pub fn new(
        chat: Rc<RefCell<ChatState>>,
        theme: &Theme,
        focused: Rc<std::cell::Cell<bool>>,
        branch_armed: Rc<RefCell<Option<String>>>,
        selection_copied: Rc<std::cell::Cell<Option<SelectionCopied>>>,
        image_store: Rc<RefCell<ImageStore>>,
    ) -> TranscriptView {
        // Caps are unknown at construction (the probe runs after `app.init`),
        // so build with the default (images off). `Shell::restyle` pushes
        // caps-aware styles once the probe lands. See [`TerminalCaps`].
        let styles = Rc::new(TranscriptStyles::from_theme(theme, TerminalCaps::default()));
        let cache = Rc::new(RefCell::new(EntryRenderCache::new()));
        let builder = EntryBuilder {
            chat: Rc::clone(&chat),
            styles: Rc::clone(&styles),
            cache: Rc::clone(&cache),
            focus_mode: Rc::clone(&focused),
            branch_armed: Rc::clone(&branch_armed),
            copy_label: Rc::new(copy_label_spans(&styles)),
            branch_label: Rc::new(branch_label_spans(&styles)),
            image_store: Rc::clone(&image_store),
        };
        let mut list = ListView::new(Source::Builder(Box::new(builder)));
        // `draw_cursor` stays off in every mode: the focused-message marker is
        // the border painted into the bubble padding (Spec E section 2), not a
        // cursor gutter. The list cursor still exists and moves under focus
        // navigation. `draw_cursor` only controls the gutter drawing.
        list.draw_cursor = false;
        // Give the transcript a terminal-scrollback feel: when the content is
        // shorter than the chat slot it sits at the bottom, so the first message
        // lands right above the editor and later ones grow upward.
        list.anchor_short_to_bottom = true;
        let bars = ScrollBars::new(list);
        bars.borrow_mut().draw_horizontal_scrollbar = false;
        apply_scrollbar_thumbs(&mut bars.borrow_mut(), &styles);
        let list = Rc::clone(&bars.borrow().view);
        let last_globals = GlobalRenderInputs::read(&chat.borrow());
        TranscriptView {
            chat,
            list,
            bars,
            cache,
            styles,
            image_store,
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
            branch_armed,
            on_exit_focus: None,
            on_observe_agent: None,
            agent_click: None,
            agent_hit_rows: Vec::new(),
            agent_hit_width: 0,
            selection: None,
            selection_origin: None,
            selection_unit: SelectionUnit::Character,
            last_click: None,
            selection_copied,
            last_view: Size {
                width: 0,
                height: 0,
            },
            me: Weak::new(),
            scroll_anim: None,
            scroll_tick_scheduled: false,
        }
    }

    /// Re-engage follow-tail so the next draw pins the viewport to the bottom,
    /// and drop the view state a swap or a switch invalidates.
    ///
    /// Not what keeps a new model off the old one's cached surfaces: the caches
    /// retire an incarnation's slots on their own (see
    /// [`EntryRenderCache::retire`]). What a caller that forgets this loses is
    /// view state, and not only the scroll position: the selection stays
    /// anchored in entries that are gone, and the list's geometry is keyed by
    /// index, so a swap to content of the same length keeps the outgoing
    /// entries' measured heights and self-heals only as rows redraw.
    ///
    /// Two callers use it. On a session rebuild the view's `chat` cell keeps
    /// its identity across the swap (the outer loop overwrites its contents
    /// in place), so the fresh session's transcript must open at the tail
    /// rather than wherever the previous session was scrolled. On an
    /// `active_view` switch each view opens at its bottom (Spec E section 1,
    /// per-view scroll). The draw path refreshes `item_count` before
    /// scrolling, so we needn't touch the list's scroll offset here.
    pub(crate) fn reset_to_tail(&mut self) {
        // A different session in the reused `chat` cell is a different
        // incarnation of the model, and the caches retire an incarnation's
        // slots themselves now (see `EntryRenderCache::retire`), so this clear
        // is not what keeps a fresh session off the previous one's surface.
        // What it is still for is the other caller: a view switch, which is the
        // same incarnation and so passes the retirement untouched. The render
        // cache keys views apart (`AgentId` is in the key) and the draw's
        // global-input clear catches the switch anyway, so this one is
        // belt-and-braces.
        self.cache.borrow_mut().clear();
        // The text cache is the one that needs it. Its key is an `EntryId`
        // alone, so two views collide outright, and select-to-copy reaches it
        // without drawing, so the draw's global-input clear cannot be what
        // covers the switch. Keying it by `(AgentId, EntryId)` would retire
        // this clear the way the retirement retired the one above.
        self.entry_text.clear();
        self.agent_click = None;
        self.agent_hit_rows.clear();
        self.agent_hit_width = 0;
        // The reused list holds a different session whose entries reuse indices,
        // so a geometry carried over from the previous session would missize the
        // new session's thumb. Drop it so the next draw rebuilds it.
        self.list.borrow_mut().reset_geometry();
        self.follow_tail = true;
        // A fresh session's entries are unrelated to the old selection's anchor
        // entry, so drop it rather than highlight stale content.
        self.selection = None;
        self.selection_origin = None;
        self.last_click = None;
        // An in-flight glide targets a position in the outgoing content, which
        // the swap invalidates (a focus glide by entry index, a page glide by
        // line delta). Cancel it so its remaining scroll does not land on the
        // new session's transcript.
        self.scroll_anim = None;
    }

    /// Install the callback invoked when Esc leaves transcript-focus mode.
    /// The host wires it to move focus back to the editor (see `Shell::new`),
    /// whose `FocusOut` then clears the item cursor and exits the mode.
    pub(crate) fn set_on_exit_focus(&mut self, on_exit: Box<dyn FnMut(&mut EventContext)>) {
        self.on_exit_focus = Some(on_exit);
    }

    /// Install the callback invoked after a plain click on a sub-agent box.
    pub(crate) fn set_on_observe_agent(&mut self, on_observe: Box<dyn FnMut(AgentId)>) {
        self.on_observe_agent = Some(on_observe);
    }

    /// Cancel any in-flight sub-agent box click.
    pub(crate) fn cancel_agent_click(&mut self) {
        self.cancel_selection_gesture();
    }

    fn cancel_selection_gesture(&mut self) {
        self.agent_click = None;
        self.selection_origin = None;
        self.selection_unit = SelectionUnit::Character;
        self.last_click = None;
    }

    /// Record the view's own `WidgetRef` (as a `Weak`), so focus navigation can
    /// schedule ticks targeting this widget to drive the smooth scroll. The
    /// host calls this once the view is behind an `Rc`. Without it a focus step
    /// snaps rather than animating.
    pub(crate) fn set_widget_ref(&mut self, me: Weak<RefCell<TranscriptView>>) {
        self.me = me;
    }

    /// This view's `WidgetRef`, for self-scheduled ticks. `None` when the weak
    /// self-reference is unset (unit tests) or has been dropped.
    fn widget(&self) -> Option<WidgetRef> {
        self.me.upgrade().map(|rc| -> WidgetRef { rc })
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

    #[cfg(test)]
    pub(crate) fn is_at_bottom(&self) -> bool {
        self.list.borrow().is_at_bottom()
    }

    #[cfg(test)]
    pub(crate) fn is_following_tail(&self) -> bool {
        self.follow_tail
    }

    #[cfg(test)]
    pub(crate) fn has_selection(&self) -> bool {
        self.selection.is_some()
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

    /// Move the item cursor onto entry `idx` and bring it into view, gliding
    /// the viewport there rather than snapping (see [`ScrollAnim`]). The
    /// destination keeps [`FOCUS_SCROLL_MARGIN`] rows of context on the side
    /// scrolled from (see [`FocusAnchor`]), so the surrounding replies stay on
    /// screen and the message never sits flush against a viewport edge.
    /// `idx` must be a valid entry index. `item_count` is refreshed first so
    /// the move sees the current length even before the focused view's first
    /// draw.
    fn focus_item(&mut self, ctx: &mut EventContext, idx: usize) {
        let count = self.entry_count();
        {
            let mut list = self.list.borrow_mut();
            list.item_count = Some(u32::try_from(count).expect("entry count fits u32"));
            // Move the cursor at once so the focus border lands on the target
            // immediately; only the viewport lags behind and animates.
            list.cursor = u32::try_from(idx).expect("index fits u32");
        }
        self.start_focus_scroll(ctx, idx);
    }

    /// Plan and begin the glide toward the (already-cursored) entry `idx`.
    ///
    /// Reads the current viewport top and the target's extent from the list's
    /// geometry to decide where the viewport should rest ([`FocusAnchor`]) and
    /// how far it must travel. A travel below [`SCROLL_ANIM_MIN_LINES`], a
    /// viewport not yet measured, or a missing self-reference (unit tests) all
    /// land immediately instead of animating.
    fn start_focus_scroll(&mut self, ctx: &mut EventContext, idx: usize) {
        let count = self.entry_count();
        let margin = f64::from(FOCUS_SCROLL_MARGIN);
        let (anchor, total) = {
            let list = self.list.borrow();
            let vh = f64::from(list.viewport_height().unwrap_or(0));
            let top0 = usize::try_from(list.scroll_top()).expect("top fits usize");
            let off0 = f64::from(list.scroll_offset());
            // Absolute line of the current viewport top and the target's top /
            // bottom, all from the same estimate-based read-model.
            let l0 = line_as_f64(list.item_top_line(top0)) + off0;
            let target_top = line_as_f64(list.item_top_line(idx));
            let target_bottom = line_as_f64(list.item_top_line(idx + 1));
            let item_h = target_bottom - target_top;

            // Cap the margin by the content on that side, so a message at the
            // transcript's start or end hugs the edge rather than baring a
            // strip of blank rows nothing can fill.
            let above = list.item_top_line(idx);
            let below = list
                .item_top_line(count)
                .saturating_sub(list.item_top_line(idx + 1));
            let cap = |rows: u64| -> u16 {
                u16::try_from(u64::from(FOCUS_SCROLL_MARGIN).min(rows))
                    .unwrap_or(FOCUS_SCROLL_MARGIN)
            };
            let top_margin = cap(above);
            let bottom_margin = cap(below);

            // Choose the resting frame, keeping the (capped) margin of context
            // on the side we scrolled from. A message too tall to frame with a
            // full margin is pinned to the top so its head shows. An older
            // target (or one hugging the top) rests below the top; a newer one
            // below the fold (or hugging the bottom) rests above the bottom; an
            // already-framed one does not move.
            let (anchor, ldest) = if item_h + margin >= vh {
                (FocusAnchor::Top { margin: 0 }, target_top)
            } else if idx <= top0 || target_top - margin < l0 {
                (
                    FocusAnchor::Top { margin: top_margin },
                    target_top - f64::from(top_margin),
                )
            } else if target_bottom + margin > l0 + vh {
                (
                    FocusAnchor::Bottom {
                        margin: bottom_margin,
                    },
                    target_bottom + f64::from(bottom_margin) - vh,
                )
            } else {
                (FocusAnchor::Keep, l0)
            };
            (anchor, ldest - l0)
        };

        let viewport_unmeasured = self.list.borrow().viewport_height().is_none();
        if viewport_unmeasured || self.widget().is_none() || total.abs() < SCROLL_ANIM_MIN_LINES {
            // Nothing to animate: land now and drop any in-flight glide so it
            // cannot keep driving the viewport past this destination.
            self.scroll_anim = None;
            self.snap_focus(anchor);
            return;
        }
        self.scroll_anim = Some(ScrollAnim {
            total,
            applied: 0.0,
            completion: ScrollCompletion::Focus(anchor),
            start: Instant::now(),
            duration: scroll_anim_duration(total.abs()),
        });
        self.schedule_scroll_tick(ctx);
        ctx.redraw = true;
    }

    /// Begin a page glide of `delta` lines (negative scrolls up), the smooth
    /// counterpart to [`ListView::scroll_lines`]. A missing self-reference
    /// (unit tests) or an unmeasured viewport applies the delta at once, and a
    /// glide already at the edge it heads for is a no-op.
    ///
    /// A press during an in-flight page glide carries that glide's unfinished
    /// travel into the new one, so rapid presses reach the same cumulative
    /// position repeated instant scrolls would rather than restarting from
    /// mid-glide and under-scrolling.
    fn start_line_scroll(&mut self, ctx: &mut EventContext, delta: i32) {
        if self.widget().is_none() || self.list.borrow().viewport_height().is_none() {
            self.scroll_anim = None;
            self.list.borrow_mut().scroll_lines(delta);
            return;
        }
        // Carry an in-flight page glide's remaining travel. A focus glide's
        // travel is measured against a cursor, not the viewport top, so it is
        // discarded (the new page glide replaces it outright).
        let carried = match &self.scroll_anim {
            Some(a) if matches!(a.completion, ScrollCompletion::Page) => a.total - a.applied,
            _ => 0.0,
        };
        let total = f64::from(delta) + carried;
        if self.at_scroll_end(total < 0.0) {
            self.scroll_anim = None;
            return;
        }
        if total.abs() < SCROLL_ANIM_MIN_LINES {
            // A net move too small to bother animating (e.g. a reversal that
            // nearly cancels the carry): apply it at once and drop the glide.
            self.scroll_anim = None;
            self.list.borrow_mut().scroll_lines(round_lines(total));
            return;
        }
        self.scroll_anim = Some(ScrollAnim {
            total,
            applied: 0.0,
            completion: ScrollCompletion::Page,
            start: Instant::now(),
            duration: scroll_anim_duration(total.abs()),
        });
        self.schedule_scroll_tick(ctx);
        ctx.redraw = true;
    }

    /// Schedule the next scroll-animation frame unless one is already pending.
    /// The [`scroll_tick_scheduled`](Self::scroll_tick_scheduled) guard keeps
    /// exactly one tick chain alive across cancellation and retarget.
    fn schedule_scroll_tick(&mut self, ctx: &mut EventContext) {
        if self.scroll_tick_scheduled {
            return;
        }
        if let Some(widget) = self.widget() {
            ctx.tick(SCROLL_ANIM_FRAME_MS, widget);
            self.scroll_tick_scheduled = true;
        }
    }

    /// Cancel any in-flight glide so a manual or instant scroll owns the
    /// viewport. Any already-scheduled tick fires once, finds no animation, and
    /// stops (see [`scroll_tick_scheduled`](Self::scroll_tick_scheduled)), so
    /// this need not touch the tick flag.
    fn cancel_scroll_anim(&mut self) {
        self.scroll_anim = None;
    }

    /// Whether the viewport, as of the last draw, has reached the edge it would
    /// head for scrolling `toward_top` (or downward when false). A page glide
    /// stops here rather than spending its duration re-applying deltas the draw
    /// clamps away at the edge.
    fn at_scroll_end(&self, toward_top: bool) -> bool {
        let list = self.list.borrow();
        if toward_top {
            list.scroll_top() == 0 && list.scroll_offset() == 0
        } else {
            list.is_at_bottom()
        }
    }

    /// Land the viewport on the focused message at once, at the position the
    /// glide targets.
    ///
    /// `Top` and `Bottom` both pin the message a fixed number of rows down from
    /// the viewport top and back-fill the preceding entries above it, so the
    /// resting frame is exact regardless of the estimate the glide travelled
    /// on. `Keep` only ensures the message stays in view without reframing.
    fn snap_focus(&self, anchor: FocusAnchor) {
        let mut list = self.list.borrow_mut();
        let cursor = list.cursor;
        if let FocusAnchor::Keep = anchor {
            list.ensure_scroll();
            return;
        }
        // Recompute the message height from the now-measured layout so the
        // landing is exact regardless of the estimate the glide travelled on.
        let idx = usize::try_from(cursor).expect("cursor fits usize");
        let vh = i32::from(list.viewport_height().unwrap_or(0));
        let item_h = i32::try_from(
            list.item_top_line(idx + 1)
                .saturating_sub(list.item_top_line(idx)),
        )
        .unwrap_or(vh);
        // Rows of the preceding entries to reveal above the message. Both
        // anchors clamp against the measured height so the message never lands
        // with its bottom past the viewport: a message too tall for the margin
        // gets less context (or none, pinned to the top), staying fully in view.
        let rows_above = match anchor {
            FocusAnchor::Top { margin } => i32::from(margin).min(vh - item_h),
            FocusAnchor::Bottom { margin } => vh - i32::from(margin) - item_h,
            FocusAnchor::Keep => unreachable!("handled above"),
        };
        list.jump_to_item(cursor);
        if rows_above > 0 {
            // The draw clamps at the transcript top when there are fewer rows
            // above, so a message near the start simply gets less context.
            list.scroll_lines(-rows_above);
        }
    }

    /// Advance the in-flight glide one frame, re-arming until it finishes.
    /// Driven by `Event::Tick`. A no-op with no animation.
    ///
    /// Each frame applies only the incremental eased delta so per-frame
    /// rounding cannot drift. A focus glide finishes by snapping to its anchor
    /// (its estimate-based deltas need not have landed exactly); a page glide
    /// applies the exact remainder, and also stops early the moment the
    /// viewport reaches the edge it heads for.
    fn advance_scroll_anim(&mut self, ctx: &mut EventContext) {
        self.advance_scroll_anim_at(ctx, Instant::now());
    }

    /// Clock-controlled implementation used by deterministic animation tests.
    /// Production ticks pass [`Instant::now`] through the wrapper above.
    fn advance_scroll_anim_at(&mut self, ctx: &mut EventContext, now: Instant) {
        // This delivery consumes the pending tick; a continuing glide re-arms
        // below via `schedule_scroll_tick`.
        self.scroll_tick_scheduled = false;
        let Some(mut anim) = self.scroll_anim.take() else {
            return;
        };
        // A page glide that has reached its edge cannot move further, so drop
        // it rather than tick out the rest of its duration.
        if matches!(anim.completion, ScrollCompletion::Page) && self.at_scroll_end(anim.total < 0.0)
        {
            return;
        }
        let elapsed = now.saturating_duration_since(anim.start);
        let t = (elapsed.as_secs_f64() / anim.duration.as_secs_f64()).clamp(0.0, 1.0);
        if t >= 1.0 {
            match anim.completion {
                ScrollCompletion::Focus(anchor) => self.snap_focus(anchor),
                ScrollCompletion::Page => {
                    let delta = anim.total - anim.applied;
                    if delta.abs() >= 1.0 {
                        self.list.borrow_mut().scroll_lines(round_lines(delta));
                    }
                }
            }
            ctx.redraw = true;
            return;
        }
        // Ease-out cubic: fast to start, decelerating into the target.
        let eased = 1.0 - (1.0 - t).powi(3);
        let want = (anim.total * eased).round();
        let delta = want - anim.applied;
        if delta != 0.0 {
            self.list.borrow_mut().scroll_lines(round_lines(delta));
        }
        anim.applied = want;
        self.scroll_anim = Some(anim);
        self.schedule_scroll_tick(ctx);
        ctx.redraw = true;
    }

    /// Move the cursor onto the newest (last) user message. Used on entering
    /// focus mode and for the End / G jump. A no-op with no user message.
    fn focus_last_user_message(&mut self, ctx: &mut EventContext) {
        if let Some(&idx) = self.user_message_indices().last() {
            self.focus_item(ctx, idx);
        }
    }

    /// Move the cursor onto the oldest (first) user message. Home / g jump.
    /// A no-op with no user message.
    fn focus_first_user_message(&mut self, ctx: &mut EventContext) {
        if let Some(&idx) = self.user_message_indices().first() {
            self.focus_item(ctx, idx);
        }
    }

    /// Step to the next-older user message (toward index 0 from the current
    /// cursor). Clamps at the first, so a no-op once there is none older. It
    /// finds the nearest user message strictly above the cursor, which is
    /// defensive against a cursor that ever sits on a non-user entry.
    pub(crate) fn focus_prev_user_message(&mut self, ctx: &mut EventContext) {
        let cursor = usize::try_from(self.list.borrow().cursor).expect("cursor fits usize");
        if let Some(&idx) = self
            .user_message_indices()
            .iter()
            .rev()
            .find(|&&i| i < cursor)
        {
            self.focus_item(ctx, idx);
        }
    }

    /// Step to the next-newer user message (toward the end). Clamps at the
    /// last, so a no-op once there is none newer. It finds the nearest user
    /// message strictly below the cursor, defensive the same way
    /// [`focus_prev_user_message`](Self::focus_prev_user_message) is.
    fn focus_next_user_message(&mut self, ctx: &mut EventContext) {
        let cursor = usize::try_from(self.list.borrow().cursor).expect("cursor fits usize");
        if let Some(&idx) = self.user_message_indices().iter().find(|&&i| i > cursor) {
            self.focus_item(ctx, idx);
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

    /// The stable message id of the focused user message, the branch anchor
    /// for the `b` shortcut. Sibling of [`focused_message_text`].
    ///
    /// `Some` only when in focus mode, the cursor sits on an `EntryKind::User`
    /// entry, and the active view is Main. A sub-agent user message is not a
    /// branch point: its parent chain lives on a sub thread, so anchoring the
    /// user-thread head there would splice the main conversation onto a
    /// sub-agent thread. We gate the Main-view check here (rather than in the
    /// caller) because `chat.active_view()` is already in hand.
    pub(crate) fn focused_message_id(&self) -> Option<String> {
        if !self.in_focus_mode() {
            return None;
        }
        let chat = self.chat.borrow();
        if chat.active_view() != AgentId::Main {
            return None;
        }
        let idx = usize::try_from(self.list.borrow().cursor).ok()?;
        let entry = chat.transcript(chat.active_view())?.entries().get(idx)?;
        match &entry.kind {
            // A user row with no durable id (nothing in the TUI produces
            // one, but the type allows it) is not a branch anchor: there
            // is no log entry to branch from.
            EntryKind::User(user) => user.message_id.clone(),
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
        self.focus_last_user_message(ctx);
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
            self.focus_prev_user_message(ctx);
            ctx.consume_and_redraw();
        } else if key.matches(Key::DOWN, empty)
            || key.matches(u32::from('j'), empty)
            || key.matches(u32::from('n'), ctrl)
            || key.matches(Key::TAB, Modifiers::SHIFT)
        {
            self.focus_next_user_message(ctx);
            ctx.consume_and_redraw();
        } else if key.matches(u32::from('g'), empty) {
            self.focus_first_user_message(ctx);
            ctx.consume_and_redraw();
        } else if key.matches(u32::from('g'), Modifiers::SHIFT) {
            self.focus_last_user_message(ctx);
            ctx.consume_and_redraw();
        } else if key.matches(Key::ESCAPE, empty) {
            // Leave focus at the live bottom: a clean resting position to hand
            // back to the editor. Re-engage follow-tail so the next draw (out
            // of focus mode by then, via the `FocusOut` the exit callback
            // triggers) pins the viewport there, and drop any in-flight glide
            // so it cannot fight the re-pin. Gated on having an exit callback:
            // without one there is no `FocusOut` to leave the mode, so we must
            // not strand `follow_tail` on inside focus mode. This lands the
            // exit at the bottom only for Esc: an overlay stealing focus exits
            // through `exit_focus_mode` alone and leaves the scroll where it was.
            if self.on_exit_focus.is_some() {
                self.resume_follow_tail();
            }
            if let Some(on_exit) = self.on_exit_focus.as_mut() {
                on_exit(ctx);
            }
            ctx.consume_and_redraw();
        }
    }

    /// Scroll the transcript up by half a viewport (Spec E section 1, the
    /// PageUp chord).
    ///
    /// A manual scroll up means the reader wants history, so follow-tail
    /// disengages and new content stops yanking the viewport to the bottom.
    /// (Paging up on a transcript that fits the viewport can't move it, so
    /// the draw re-engages follow-tail right away, leaving it pinned.)
    pub(crate) fn page_up(&mut self, ctx: &mut EventContext) {
        self.follow_tail = false;
        // Read the viewport under a short immutable borrow that drops at the
        // end of the statement, before `start_line_scroll` borrows again.
        let lines = crate::scroll::half_page_scroll_lines(self.list.borrow().viewport_height());
        self.start_line_scroll(ctx, -lines);
    }

    /// Scroll the transcript down by half a viewport (Spec E section 1, the
    /// PageDown chord).
    ///
    /// This never touches `follow_tail` directly. If the glide lands back at
    /// the bottom the next draw re-engages follow-tail (see
    /// [`draw`](Widget::draw)), so paging down to the end resumes following
    /// streamed content.
    pub(crate) fn page_down(&mut self, ctx: &mut EventContext) {
        let lines = crate::scroll::half_page_scroll_lines(self.list.borrow().viewport_height());
        self.start_line_scroll(ctx, lines);
    }

    /// Scroll the transcript to the top (Spec E section 1, Home), mode-aware.
    pub(crate) fn scroll_to_top(&mut self, ctx: &mut EventContext) {
        if self.in_focus_mode() {
            // Focus mode: move the item cursor onto the first user message,
            // matching the `g` jump.
            self.focus_first_user_message(ctx);
            return;
        }
        // Reaching the top means the reader left the tail, so follow-tail
        // disengages. `jump_to_item(0)` pins the scroll window to item 0 at
        // offset 0, the very first line, rather than only moving the hidden
        // cursor. Cancel any in-flight glide so it cannot drive the viewport
        // back off the top afterward.
        self.cancel_scroll_anim();
        self.follow_tail = false;
        self.list.borrow_mut().jump_to_item(0);
    }

    /// Scroll the transcript to the bottom (Spec E section 1, End), mode-aware.
    pub(crate) fn scroll_to_bottom(&mut self, ctx: &mut EventContext) {
        if self.in_focus_mode() {
            // Focus mode: move the item cursor onto the last user message,
            // matching the `G` jump.
            self.focus_last_user_message(ctx);
            return;
        }
        // Re-engaging follow-tail is the whole gesture: the next draw runs the
        // inner list's `scroll_to_bottom` and the transcript resumes following
        // the tail (see [`draw`](Widget::draw)). Cancel any in-flight glide so
        // it does not fight the re-pin.
        self.resume_follow_tail();
    }

    /// Handles Escape after focused widgets and overlays decline it.
    ///
    /// A live selection owns the first key. Otherwise, Escape resumes a
    /// detached viewport and leaves an already-following transcript unclaimed.
    pub(crate) fn handle_unfocused_escape(&mut self) -> bool {
        if self.in_focus_mode() {
            return false;
        }
        if self.selection.take().is_some() {
            return true;
        }
        if self.follow_tail {
            return false;
        }
        self.resume_follow_tail();
        true
    }

    /// Re-engages follow-tail so the next draw pins the transcript to the bottom.
    pub(crate) fn resume_follow_tail(&mut self) {
        self.cancel_scroll_anim();
        self.follow_tail = true;
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
            branch_armed: Rc::clone(&self.branch_armed),
            copy_label: Rc::new(copy_label_spans(&styles)),
            branch_label: Rc::new(branch_label_spans(&styles)),
            image_store: Rc::clone(&self.image_store),
        };
        self.list.borrow_mut().children = Source::Builder(Box::new(builder));
        apply_scrollbar_thumbs(&mut self.bars.borrow_mut(), &styles);
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
        let (generation, fingerprint) = {
            let chat = self.chat.borrow();
            let agent = chat.active_view();
            match chat.transcript(agent).and_then(|t| t.get(id)) {
                Some(entry) => (chat.generation(), entry_fingerprint(entry, &chat)),
                None => return Rc::new(Vec::new()),
            }
        };
        if let Some(rows) = self.entry_text.get(id, generation, fingerprint, width) {
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
            // `entry_rows` feeds the select-to-copy measurement, so an image
            // renders as its `[image: ...]` text (copying over an image yields
            // that text, not a blank reserve). Force `Disabled` here.
            let mut widget = build_entry_widget(
                entry,
                &chat,
                &self.styles,
                false,
                None,
                ImageRender::Disabled,
            )
            .into_indented_boxed();
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
            .insert(id, generation, fingerprint, width, Rc::clone(&rows));
        rows
    }

    /// Rendered-row count of entry `id` at `width`.
    ///
    /// Usually at least 1, but a hidden token-usage row (see
    /// [`build_entry_widget`]) renders zero rows. The per-entry walks must see
    /// that true zero, otherwise their row accounting drifts by one against the
    /// composited list for every entry below a hidden row.
    fn entry_height(&mut self, id: EntryId, width: u16) -> usize {
        self.entry_rows(id, width).len()
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
        // NOTE: the bars self-stamp their surface, so the bus can reach them
        // directly, yet we still forward here on purpose. Forwarding lets the
        // transcript branch on the bars' consume result within the same event:
        // a thumb drag drops follow-tail and cancels the glide, and the content
        // selection below runs only when the bars declined, so grabbing the
        // thumb scrolls rather than selects. Relying on bus routing alone would
        // force us to infer the drag from state a prior event set, which is
        // subtler for no gain.
        //
        // This forward is not double dispatch. The transcript is an ancestor of
        // the bars in the hit path, so during a drag this capture-phase forward
        // consumes the event before the bus's capture walk descends to the
        // bars, and a thumb press is consumed at the bars target before the
        // bubble phase climbs back to the transcript. A motion neither consumes
        // reaches the bars' handlers twice, once via the bus and once here, but
        // those are idempotent (hover only tracks position, capture is inert
        // unless dragging), so there is no double effect.
        self.bars.borrow_mut().capture_event(ctx, event);
        if ctx.consume_event {
            self.cancel_selection_gesture();
            if m.kind == mouse::Type::Drag {
                // A thumb drag moves the viewport, so it takes over from any
                // in-flight glide and leaves the tail.
                self.cancel_scroll_anim();
                self.follow_tail = false;
            }
            return;
        }
        if matches!(m.button, mouse::Button::WheelUp | mouse::Button::WheelDown) {
            // A wheel tick is a manual scroll, so it supersedes any glide.
            self.cancel_scroll_anim();
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
    /// We clamp `m_row` into the viewport, drop the blank top band that
    /// bottom-anchoring leaves above the first entry, add the top entry's
    /// hidden-line offset to get the line from the top entry's start, then walk
    /// realized entries top to bottom (each at most a viewport of rows) until
    /// that line lands inside one. `col` is `m_col` clamped into `[0, width]`
    /// (the scrollbar owns the last column, the far edge is end-of-line).
    /// Returns `None` on an empty transcript, where there is nothing to select.
    fn point_to_sel(&mut self, m_row: i16, m_col: i16) -> Option<SelPos> {
        let width = self.content_width();
        let (top_idx, off, top_pad) = {
            let list = self.list.borrow();
            (
                usize::try_from(list.scroll_top()).unwrap_or(0),
                list.scroll_offset(),
                i32::from(list.top_pad()),
            )
        };
        let height = i32::from(self.last_view.height);
        let local_row = i32::from(m_row).clamp(0, (height - 1).max(0));
        // Map the screen row to a content row by dropping the blank top band
        // bottom-anchoring leaves above the first entry. A row inside that band
        // (`local_row < top_pad`) resolves to the first content line rather
        // than a spurious selection above the text. `top_pad` is 0 in the
        // normal top-anchored case, so this is a no-op there.
        let content_row = (local_row - top_pad).max(0);
        // The line from the top entry's first line: the hidden-above offset
        // plus how far down the content the click landed.
        let target = usize::try_from(off)
            .unwrap_or(0)
            .saturating_add(usize::try_from(content_row).unwrap_or(0));
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

    /// Return the sub-agent whose box contains this point in the last frame.
    fn subagent_at_point(&self, row: i16, col: i16) -> Option<AgentId> {
        let row = usize::try_from(row).ok()?;
        let col = u16::try_from(col).ok()?;
        if col >= self.agent_hit_width {
            return None;
        }
        self.agent_hit_rows.get(row).copied().flatten()
    }

    fn click_count(&mut self, pos: SelPos, now: Instant) -> u8 {
        let count = self.last_click.map_or(1, |last| {
            let nearby = last.pos.entry == pos.entry
                && last.pos.line == pos.line
                && last.pos.col.abs_diff(pos.col) <= 1;
            if nearby
                && now
                    .checked_duration_since(last.at)
                    .is_some_and(|elapsed| elapsed <= MULTI_CLICK_INTERVAL)
                && last.count < 3
            {
                last.count + 1
            } else {
                1
            }
        });
        self.last_click = Some(LastClick {
            at: now,
            pos,
            count,
        });
        count
    }

    fn selection_for_unit(&mut self, pos: SelPos, unit: SelectionUnit) -> Selection {
        match unit {
            SelectionUnit::Character => Selection {
                anchor: pos,
                caret: pos,
            },
            SelectionUnit::Word => self.word_at(pos),
            SelectionUnit::Line => self.line_at(pos),
        }
    }

    fn line_at(&mut self, pos: SelPos) -> Selection {
        let rows = self.entry_rows(pos.entry, self.content_width());
        if pos.line >= rows.len() {
            return Selection {
                anchor: pos,
                caret: pos,
            };
        }
        Selection {
            anchor: SelPos {
                entry: pos.entry,
                line: pos.line,
                col: 0,
            },
            caret: SelPos {
                entry: pos.entry,
                line: pos.line,
                col: usize::from(self.content_width()),
            },
        }
    }

    fn word_at(&mut self, pos: SelPos) -> Selection {
        let rows = self.entry_rows(pos.entry, self.content_width());
        if pos.line >= rows.len() {
            return Selection {
                anchor: pos,
                caret: pos,
            };
        }

        let mut cells = Vec::new();
        let row = &rows[pos.line];
        let mut col = content_start(row);
        let end = content_end(row);
        while col < end {
            let cell = &row[col];
            let width = usize::from(cell.char.width.max(1));
            let next = col.saturating_add(width).min(row.len());
            let class = word_class(cell.char.grapheme());
            cells.push((
                SelPos {
                    entry: pos.entry,
                    line: pos.line,
                    col,
                },
                SelPos {
                    entry: pos.entry,
                    line: pos.line,
                    col: next,
                },
                class,
            ));
            col = next;
        }

        let Some(mut index) = cells
            .iter()
            .position(|(start, end, _)| pos >= *start && pos < *end)
        else {
            return Selection {
                anchor: pos,
                caret: pos,
            };
        };
        let class = cells[index].2;
        let mut end = index;
        while index > 0 && cells[index - 1].2 == class {
            index -= 1;
        }
        while end + 1 < cells.len() && cells[end + 1].2 == class {
            end += 1;
        }
        Selection {
            anchor: cells[index].0,
            caret: cells[end].1,
        }
    }

    fn extend_unit_selection(origin: Selection, target: Selection) -> Selection {
        if target.caret <= origin.anchor {
            Selection {
                anchor: origin.caret,
                caret: target.anchor,
            }
        } else if target.anchor >= origin.caret {
            Selection {
                anchor: origin.anchor,
                caret: target.caret,
            }
        } else {
            origin
        }
    }

    /// Snapshot the visible sub-agent boxes from the geometry just drawn.
    fn record_agent_hit_map(&mut self) {
        let height = usize::from(self.last_view.height);
        let width = self.content_width();
        let positions = self.drawn_row_positions(height);
        let mut rows = Vec::with_capacity(height);

        for pos in positions {
            let Some(pos) = pos else {
                rows.push(None);
                continue;
            };
            let chat = self.chat.borrow();
            let agent = chat
                .transcript(chat.active_view())
                .and_then(|transcript| transcript.get(pos.entry))
                .and_then(|entry| match &entry.kind {
                    EntryKind::SubAgent(sub) => Some(AgentId::Sub(sub.child)),
                    _ => None,
                });
            rows.push(agent);
        }

        self.agent_hit_rows = rows;
        self.agent_hit_width = width;
    }

    /// Drive the free-form selection from a left-button mouse event (Spec E
    /// section 2). Called only after the bars declined the event, so a
    /// scrollbar-thumb drag scrolls rather than selects.
    fn handle_selection_mouse(&mut self, ctx: &mut EventContext, m: &mouse::Mouse) {
        match m.kind {
            mouse::Type::Press => {
                let Some(pos) = self.point_to_sel(m.row, m.col) else {
                    return;
                };
                let count = if m.mods.is_empty() {
                    self.click_count(pos, Instant::now())
                } else {
                    self.last_click = None;
                    1
                };
                self.agent_click = if count == 1 && m.mods.is_empty() {
                    self.subagent_at_point(m.row, m.col)
                } else {
                    None
                };
                self.selection_unit = match count {
                    2 => SelectionUnit::Word,
                    3 => SelectionUnit::Line,
                    _ => SelectionUnit::Character,
                };
                let selection = self.selection_for_unit(pos, self.selection_unit);
                self.selection = Some(selection);
                self.selection_origin = Some(selection);
                self.follow_tail = false;
                ctx.redraw = true;
            }
            mouse::Type::Drag => {
                self.agent_click = None;
                self.last_click = None;
                // Dragging past the top or bottom edge auto-scrolls by the
                // overshoot so a selection can span more than one screen. The
                // revealed rows extend the selection on subsequent frames. A
                // manual drag-scroll supersedes any in-flight glide.
                let height = i16::try_from(self.last_view.height).unwrap_or(i16::MAX);
                if m.row < 0 {
                    self.cancel_scroll_anim();
                    self.list.borrow_mut().scroll_lines(i32::from(m.row));
                } else if m.row >= height {
                    self.cancel_scroll_anim();
                    self.list
                        .borrow_mut()
                        .scroll_lines(i32::from(m.row - height + 1));
                }
                let Some(caret) = self.point_to_sel(m.row, m.col) else {
                    ctx.redraw = true;
                    return;
                };
                let target = self.selection_for_unit(caret, self.selection_unit);
                match self.selection_origin {
                    Some(origin) => {
                        self.selection = Some(Self::extend_unit_selection(origin, target));
                    }
                    // A drag with no prior press is not expected, but start a
                    // selection at the caret rather than drop the interaction.
                    None => {
                        self.selection = Some(target);
                        self.selection_origin = Some(target);
                    }
                }
                ctx.redraw = true;
            }
            mouse::Type::Release => {
                let released_agent = if m.mods.is_empty() {
                    self.subagent_at_point(m.row, m.col)
                } else {
                    None
                };
                let observe = self
                    .agent_click
                    .take()
                    .filter(|id| Some(*id) == released_agent);
                if let Some(sel) = self.selection {
                    if sel.anchor == sel.caret {
                        // A plain click (no drag) clears the selection.
                        self.selection = None;
                    } else {
                        // Select-to-copy: a real range copies to the clipboard
                        // via OSC 52 and stays highlighted until the next click
                        // or Esc. A range that covers only blank margin extracts
                        // nothing, so we neither copy nor toast (the clipboard
                        // is left untouched).
                        let width = self.content_width();
                        let text = self.extract_selection(width, sel.anchor, sel.caret);
                        if !text.is_empty() {
                            // Report the copy to the toast. Count graphemes, so
                            // a multi-byte character (or an emoji) reads as one.
                            let chars = text.graphemes(true).count();
                            self.selection_copied.set(Some(SelectionCopied {
                                chars,
                                at: Instant::now(),
                            }));
                            ctx.copy_to_clipboard(text);
                        }
                    }
                    ctx.redraw = true;
                }
                self.selection_origin = None;
                self.selection_unit = SelectionUnit::Character;
                if let Some(id) = observe
                    && let Some(on_observe) = self.on_observe_agent.as_mut()
                {
                    on_observe(id);
                }
            }
            mouse::Type::Motion => {}
        }
    }

    /// Clickable row positions from the list's last completed layout geometry.
    /// The trailing spacer of each entry is left unmapped.
    fn drawn_row_positions(&self, height: usize) -> Vec<Option<RowPos>> {
        let list = self.list.borrow();
        let mut idx = usize::try_from(list.scroll_top()).unwrap_or(0);
        let mut line = usize::try_from(list.scroll_offset()).unwrap_or(0);
        let pad = usize::from(list.top_pad()).min(height);
        let mut rows = vec![None; pad];

        let item_height = |idx: usize| {
            // A hidden (zero-height) entry reports a true 0 here so the walk
            // stays aligned with the composited list, which gives it no row.
            usize::try_from(
                list.item_top_line(idx + 1)
                    .saturating_sub(list.item_top_line(idx)),
            )
            .unwrap_or(usize::MAX)
        };
        let mut current = self.entry_id_at(idx).map(|id| (id, item_height(idx)));
        for _ in pad..height {
            while let Some((_, current_height)) = current {
                if line < current_height {
                    break;
                }
                idx += 1;
                line = 0;
                current = self.entry_id_at(idx).map(|id| (id, item_height(idx)));
            }
            match current {
                Some((entry, current_height)) if line + 1 < current_height => {
                    rows.push(Some(RowPos { entry, line }));
                    line += 1;
                }
                Some(_) => {
                    rows.push(None);
                    line += 1;
                }
                None => rows.push(None),
            }
        }
        rows
    }

    /// For each of the `height` visible screen rows, the `(entry, line)` it
    /// displays, or `None` for a row past the end of content.
    ///
    /// Walks realized entries once from the top, so it is O(viewport) rather
    /// than O(viewport * entries). The top entry's hidden-above offset seeds
    /// the starting line within that entry. When bottom-anchoring leaves a
    /// blank band above the first entry, that band's rows come out as `None`.
    fn visible_row_positions(&mut self, height: usize) -> Vec<Option<RowPos>> {
        let width = self.content_width();
        let (top_idx, off, top_pad) = {
            let list = self.list.borrow();
            (
                usize::try_from(list.scroll_top()).unwrap_or(0),
                usize::try_from(list.scroll_offset()).unwrap_or(0),
                usize::from(list.top_pad()),
            )
        };
        let mut out: Vec<Option<RowPos>> = Vec::with_capacity(height);
        // Bottom-anchoring leaves `top_pad` blank rows above the first entry.
        // Those screen rows show no content, so emit them as `None` before
        // walking the entries. `top_pad` is 0 in the normal top-anchored case.
        let pad = top_pad.min(height);
        for _ in 0..pad {
            out.push(None);
        }
        let mut idx = top_idx;
        // The first content row shows the top entry's line `off`, the rows
        // before it being hidden above the top edge.
        let mut line = off;
        // The current entry and its height, refreshed as the walk crosses
        // entries. `entry_id_at` and `entry_height` each take a short
        // `self.chat` borrow, so we call them one after another, never nested.
        let mut current = match self.entry_id_at(idx) {
            Some(id) => Some((id, self.entry_height(id, width))),
            None => None,
        };
        for _ in 0..(height - pad) {
            // Advance past any entries the running line has walked off the end
            // of. A hidden (zero-height) entry is consumed here too, so a screen
            // row may cross more than one entry, but the walk stays bounded by
            // the entry count.
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
            // Semantic selection: don't highlight the chrome indent margin, so
            // the painted span matches the copied text.
            let from = from.max(content_start(row));
            let (from, to) = expand_to_graphemes(row, from, to);
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
        let bars_surface = self.bars.borrow_mut().draw(ctx);
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
        // Record the viewport and clickable rows from this completed layout.
        // Mouse events dispatch against this frame until another draw replaces
        // it, even if live entry geometry changes in the meantime.
        self.last_view = ctx.max.size();
        self.record_agent_hit_map();
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
            if m.kind == mouse::Type::Drag {
                self.agent_click = None;
            }
            self.observe_mouse(ctx, event, m);
        }
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            Event::Mouse(m) => {
                if !matches!(m.button, mouse::Button::Left | mouse::Button::None) {
                    self.cancel_selection_gesture();
                }
                self.observe_mouse(ctx, event, m);
                if ctx.consume_event {
                    return;
                }
                // Thumb hover and press-to-drag live in the bars'
                // bubbling-phase handler.
                self.bars.borrow_mut().handle_event(ctx, event);
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
            Event::MouseLeave => {
                self.cancel_selection_gesture();
                self.bars.borrow_mut().handle_event(ctx, event);
            }
            // Focus in/out drive transcript-focus mode: the transcript is "in
            // focus mode" exactly when it is the focused widget (Spec E
            // section 1). FocusOut also fires when an opening overlay steals
            // focus, which cleanly exits the mode.
            Event::FocusIn => self.enter_focus_mode(ctx),
            Event::FocusOut => {
                self.cancel_selection_gesture();
                self.exit_focus_mode(ctx);
            }
            // Drives the smooth focus and page glides (see `ScrollAnim`), a
            // no-op once the animation is done and the tick chain has stopped.
            Event::Tick => self.advance_scroll_anim(ctx),
            Event::KeyPress(key) => {
                // Esc clears a live selection first (Spec E section 2), before
                // the focus-mode Esc would leave the mode, so one Esc drops the
                // highlight and a second exits focus.
                if key.matches(Key::ESCAPE, Modifiers::empty()) && self.selection.is_some() {
                    self.selection = None;
                    self.cancel_selection_gesture();
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
    use aj_agent::tool::{DiffDetails, TaskKind};
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
        TranscriptStyles::from_theme(
            &Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor),
            TerminalCaps::default(),
        )
    }

    /// The mouse-selection highlight reads the theme's `TextSelectionBg`
    /// token: the macOS selection blue on light themes, a darker blue on
    /// dark themes, and distinct from the menu-cursor band (`SelectedBg`).
    #[test]
    fn selection_bg_uses_the_theme_text_selection_token() {
        use aj_app::theme::ColorMode;
        let light = TranscriptStyles::from_theme(
            &Theme::bundled_light_with_mode(ColorMode::Truecolor),
            TerminalCaps::default(),
        );
        assert_eq!(
            light.selection_bg,
            Color::Rgb([183, 211, 248]),
            "light theme uses the macOS selection blue"
        );
        let dark_theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);
        let dark = TranscriptStyles::from_theme(&dark_theme, TerminalCaps::default());
        assert_eq!(
            dark.selection_bg,
            Color::Rgb([48, 84, 128]),
            "dark theme uses the darker selection blue"
        );
        assert_ne!(light.selection_bg, dark.selection_bg);
        assert_ne!(
            dark.selection_bg,
            vaxis_color(
                dark_theme.bg_color(ThemeBg::SelectedBg),
                ColorMode::Truecolor
            ),
            "the selection color is distinct from the pick-list band"
        );
    }

    /// `from_theme` carries the `images` gate straight from `TerminalCaps`.
    #[test]
    fn from_theme_carries_images_from_caps() {
        let theme = Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor);
        let off = TranscriptStyles::from_theme(&theme, TerminalCaps::default());
        assert!(!off.images, "default caps keep images off");
        let on = TranscriptStyles::from_theme(
            &theme,
            TerminalCaps {
                images: true,
                ..TerminalCaps::default()
            },
        );
        assert!(on.images, "caps.images flows into the styles");
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
            account: None,
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
    fn assistant_markdown_rows(t: &Transcript, show_thinking: bool, width: u16) -> Vec<String> {
        let EntryKind::Assistant(a) = &t.entries()[0].kind else {
            panic!("expected an assistant entry");
        };
        let mut view = build_assistant_markdown(a, show_thinking, false, &styles());
        let surface = view.draw(&crate::test_support::draw_ctx(width, None));
        crate::test_support::rows(&surface)
    }

    /// Draw the compaction entry's `MarkdownView` at `width` and return its
    /// composited rows plus the header's (first visible cell's) style.
    fn compaction_view_rows(t: &Transcript, expanded: bool, width: u16) -> (Vec<String>, Style) {
        let EntryKind::Compaction(c) = &t.entries()[0].kind else {
            panic!("expected a compaction entry");
        };
        let view = build_compaction_markdown(c, expanded, false, &styles());
        // Render through the indented boxed path so the rows carry the shared
        // one-column left indent the transcript applies to every entry.
        let mut widget = EntryWidget::Markdown(view).into_indented_boxed();
        let surface = widget.draw(&crate::test_support::draw_ctx(width, None));
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
            message_id: None,
        }));
        let spans = entry_spans(&t.entries()[0], &styles());
        assert_eq!(joined(&spans), "\n\n");
    }

    /// A long task notification with a recognisable first line and a
    /// tail marker well past [`NOTIFICATION_COLLAPSED_LINES`].
    fn notification() -> TaskNotificationEntry {
        let mut lines = vec!["Background task #1 finished: sleep - exit code 0".to_string()];
        for i in 1..30 {
            lines.push(format!("tick {i}"));
        }
        lines.push("SECRET_TAIL_MARKER".to_string());
        TaskNotificationEntry {
            message_id: None,
            label: "sleep".to_string(),
            kind: aj_agent::message::TaskNotificationKind::Bash,
            outcome: TaskOutcome::Succeeded,
            body: lines.join("\n"),
        }
    }

    fn notification_rows(n: &TaskNotificationEntry, expanded: bool, width: u16) -> Vec<String> {
        let mut bubble = build_task_notification(n, expanded, &styles());
        let surface = bubble.draw(&crate::test_support::draw_ctx(width, None));
        crate::test_support::rows(&surface)
    }

    fn user_bubble_rows(user: &UserEntry, width: u16) -> Vec<String> {
        let mut bubble = build_user_bubble(user, &styles(), None);
        let surface = bubble.draw(&crate::test_support::draw_ctx(width, None));
        crate::test_support::rows(&surface)
    }

    #[test]
    fn user_bubble_paints_the_tint_and_drops_the_prefix() {
        let user = UserEntry {
            content: vec![UserContent::text("hello world")],
            message_id: None,
        };
        let s = styles();
        let mut bubble = build_user_bubble(&user, &s, None);
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
    fn collapsed_notification_folds_to_ten_lines_with_dim_hint() {
        let n = notification();
        let r = notification_rows(&n, false, 80);
        let body = r.join("\n");
        assert!(body.contains("Background task #1 finished"), "{r:?}");
        assert!(!body.contains("SECRET_TAIL_MARKER"), "{r:?}");
        // 10 source lines + the hint row + 2 pads + spacer.
        assert!(
            body.contains("more lines, Alt+O to expand)"),
            "hint present: {r:?}",
        );
        assert_eq!(r.len(), 10 + 1 + 2 + 1, "{r:?}");
        // The hint row is dim, styled like a tool cell's expand hint.
        let s = styles();
        let mut bubble = build_task_notification(&n, false, &s);
        let surface = bubble.draw(&crate::test_support::draw_ctx(80, None));
        let grid = crate::test_support::flatten(&surface);
        let hint_row = &grid[11];
        assert!(
            hint_row
                .iter()
                .filter(|c| !c.char.grapheme().trim().is_empty())
                .all(|c| c.style.dim),
            "hint cells are dim",
        );
    }

    #[test]
    fn notification_bubble_uses_the_outcome_tint() {
        let s = styles();
        let ctx = crate::test_support::draw_ctx(40, None);
        let bg = |outcome: TaskOutcome| {
            let entry = TaskNotificationEntry {
                message_id: None,
                label: "sleep".into(),
                kind: aj_agent::message::TaskNotificationKind::Bash,
                outcome,
                body: "exit".into(),
            };
            let surface = build_task_notification(&entry, true, &s).draw(&ctx);
            crate::test_support::flatten(&surface)[0][0].style.bg
        };
        // Success maps to the done-tool token, failure/kill to the errored
        // one. (The bundled palette may resolve both tokens to the same
        // color, so we assert the mapping, not that the tints differ.)
        assert_eq!(bg(TaskOutcome::Succeeded), s.tool_success_bg);
        assert_eq!(bg(TaskOutcome::Failed { code: Some(1) }), s.tool_error_bg);
        assert_eq!(bg(TaskOutcome::Killed), s.tool_error_bg);
    }

    #[test]
    fn expanded_notification_shows_the_full_body() {
        let n = notification();
        let r = notification_rows(&n, true, 80);
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
            message_id: None,
        };
        let s = styles();
        let label = copy_label_spans(&s);
        let ctx = crate::test_support::draw_ctx(40, None);
        let plain = build_user_bubble(&user, &s, None).draw(&ctx);
        let bordered = build_user_bubble(&user, &s, Some(&label)).draw(&ctx);
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
        let key = action_shortcut(ACTION_COPY_MESSAGE).expect("copy chord bound");
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

    /// The focus hint renders BOTH the copy and branch shortcuts, each resolved
    /// from the shared keybinding data rather than a hardcoded letter.
    #[test]
    fn focus_hint_renders_both_copy_and_branch_shortcuts_from_binding_data() {
        let user = UserEntry {
            content: vec![UserContent::text("ciao?")],
            message_id: None,
        };
        let s = styles();
        let label = copy_label_spans(&s);
        // Wide enough that the whole hint fits on the border's bottom edge.
        let ctx = crate::test_support::draw_ctx(60, None);
        let bordered = build_user_bubble(&user, &s, Some(&label)).draw(&ctx);
        let last_row = usize::from(bordered.size.height) - 2;
        let row = crate::test_support::rows(&bordered)[last_row].clone();

        let copy_key = action_shortcut(ACTION_COPY_MESSAGE).expect("copy chord bound");
        let branch_key = action_shortcut(ACTION_BRANCH_MESSAGE).expect("branch chord bound");
        assert!(
            row.contains(&format!("{copy_key} to copy")),
            "copy shortcut on the hint line: {row:?}",
        );
        assert!(
            row.contains(&format!("{branch_key} to branch")),
            "branch shortcut on the hint line: {row:?}",
        );
    }

    #[test]
    fn long_user_message_is_never_folded() {
        let lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        let user = UserEntry {
            content: vec![UserContent::text(lines.join("\n"))],
            message_id: None,
        };
        let r = user_bubble_rows(&user, 80);
        let body = r.join("\n");
        assert!(body.contains("line 29"), "{r:?}");
        assert!(!body.contains("to expand"), "{r:?}");
    }

    #[test]
    fn short_notification_is_not_truncated() {
        let n = TaskNotificationEntry {
            message_id: None,
            label: "sleep".into(),
            kind: aj_agent::message::TaskNotificationKind::Bash,
            outcome: TaskOutcome::Succeeded,
            body: "task #1 done".into(),
        };
        let r = notification_rows(&n, false, 80);
        let body = r.join("\n");
        assert!(body.contains("task #1 done"), "{r:?}");
        assert!(!body.contains("to expand"), "{r:?}");
    }

    #[test]
    fn assistant_entry_renders_blocks_in_order_and_skips_tool_calls() {
        let t = transcript_with(EntryKind::Assistant(AssistantEntry {
            message_id: None,
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
        let rows = assistant_markdown_rows(&t, true, 80);
        // Segments stack in order with one blank row between them and the
        // trailing spacer, the tool call contributing nothing.
        assert_eq!(rows, vec!["Thinking: pondering", "", "answer", ""]);
    }

    /// An assistant entry gets the shared one-column indent when boxed, like
    /// the notice and compaction entries. The raw markdown starts flush at
    /// column zero (see the test above); the indent is added by
    /// `into_indented_boxed`.
    #[test]
    fn assistant_entry_is_indented_one_column_when_boxed() {
        let t = transcript_with(EntryKind::Assistant(AssistantEntry {
            message_id: None,
            message: assistant_message(vec![AssistantContent::Text(TextContent {
                text: "answer".into(),
                text_signature: None,
            })]),
            finalized: true,
        }));
        let EntryKind::Assistant(a) = &t.entries()[0].kind else {
            panic!("expected an assistant entry");
        };
        let view = build_assistant_markdown(a, false, false, &styles());
        let mut widget = EntryWidget::Markdown(view).into_indented_boxed();
        let surface = widget.draw(&crate::test_support::draw_ctx(40, None));
        let rows = crate::test_support::rows(&surface);
        assert_eq!(rows[0], " answer", "assistant text at column one: {rows:?}");
    }

    #[test]
    fn hidden_thinking_renders_placeholder() {
        let t = transcript_with(EntryKind::Assistant(AssistantEntry {
            message_id: None,
            message: assistant_message(vec![AssistantContent::Thinking(ThinkingContent {
                thinking: "secret".into(),
                thinking_signature: None,
                redacted: false,
            })]),
            finalized: true,
        }));
        let rows = assistant_markdown_rows(&t, false, 80);
        // A collapsed block with a body advertises the expand chord, resolved
        // from the shared binding data (the thinking-toggle chord, alt+t by
        // default), not the tools-expand chord (alt+o).
        let key = action_shortcut(ACTION_THINKING_TOGGLE).unwrap();
        assert_eq!(
            rows,
            vec![format!("Thinking… ({key} to expand)"), String::new()]
        );
        let tools_key = action_shortcut(aj_app::keybindings::ACTION_TOOLS_EXPAND).unwrap();
        assert_ne!(key, tools_key, "the two chords differ by default");
        assert!(
            !rows[0].contains(&tools_key),
            "the hint uses the thinking-toggle chord, not tools-expand: {rows:?}",
        );
    }

    #[test]
    fn hidden_thinking_without_body_suppresses_the_hint() {
        // A body-less collapsed block has nothing to reveal, so it stays a bare
        // placeholder with no expand hint.
        let t = transcript_with(EntryKind::Assistant(AssistantEntry {
            message_id: None,
            message: assistant_message(vec![AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: None,
                redacted: false,
            })]),
            finalized: true,
        }));
        let rows = assistant_markdown_rows(&t, false, 80);
        assert_eq!(rows, vec!["Thinking…", ""]);
    }

    #[test]
    fn redacted_thinking_renders_marker_even_when_expanded() {
        let t = transcript_with(EntryKind::Assistant(AssistantEntry {
            message_id: None,
            message: assistant_message(vec![AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: Some("opaque".into()),
                redacted: true,
            })]),
            finalized: true,
        }));
        let rows = assistant_markdown_rows(&t, true, 80);
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
                entry: None,
            }));
            let spans = entry_spans(&t.entries()[0], &s);
            assert_eq!(spans[0].style, style);
        }
    }

    /// Notice and usage rows get the shared one-column left indent when boxed,
    /// lining them up with the tool bubbles' content column. The spans
    /// themselves carry no leading space, the indent is a blank first column
    /// from the entry's `Padding` wrapper.
    #[test]
    fn notice_and_usage_rows_are_inset_one_column() {
        let s = styles();
        let t = transcript_with(EntryKind::Notice(NoticeEntry {
            level: NoticeLevel::Info,
            text: "note".into(),
            entry: None,
        }));
        // The raw spans carry no inset.
        let spans = entry_spans(&t.entries()[0], &s);
        assert_eq!(joined(&spans), "note\n\n");
        // Boxed, the text starts at column one over a blank first column.
        let rows = indented_rich_rows(&t.entries()[0], &s, 40);
        assert_eq!(rows[0], " note", "text at column one: {rows:?}");

        let t = transcript_with(EntryKind::TurnUsage(aj_app::chat::TurnUsageEntry {
            agent_id: aj_agent::events::AgentId::Main,
            after_message_id: None,
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
        let spans = entry_spans(&t.entries()[0], &s);
        assert!(joined(&spans).starts_with("Token Usage"), "{spans:?}");
        let rows = indented_rich_rows(&t.entries()[0], &s, 40);
        assert!(rows[0].starts_with(" Token Usage"), "{rows:?}");
    }

    /// Render an entry that goes through the `Rich` arm of the builder
    /// (notice, usage, sub-agent stub) via the indented boxed path, returning
    /// its rows.
    fn indented_rich_rows(entry: &Entry, styles: &TranscriptStyles, width: u16) -> Vec<String> {
        let mut widget =
            EntryWidget::Rich(RichText::new(entry_spans(entry, styles))).into_indented_boxed();
        let surface = widget.draw(&crate::test_support::draw_ctx(width, None));
        crate::test_support::rows(&surface)
    }

    /// A notice's SGR strikethrough markers are parsed into struck spans: the
    /// wrapped run is struck while the surrounding text is not. This fails if
    /// the Notice arm stops parsing markers and emits a single literal span.
    #[test]
    fn notice_strikethrough_markers_become_struck_spans() {
        let t = transcript_with(EntryKind::Notice(NoticeEntry {
            level: NoticeLevel::Info,
            text: "pre \x1b[9mstruck\x1b[29m post".into(),
            entry: None,
        }));
        let spans = entry_spans(&t.entries()[0], &styles());
        let struck = spans
            .iter()
            .find(|s| s.text == "struck")
            .expect("the struck run is its own span");
        assert!(struck.style.strikethrough, "the marked run renders struck");
        for s in &spans {
            if s.text.contains("pre") || s.text.contains("post") {
                assert!(
                    !s.style.strikethrough,
                    "text outside the markers is not struck: {s:?}"
                );
            }
        }
    }

    /// A realistic context notice in the shape `build_context_notice` emits
    /// (the `  - ` bullet outside the strike markers, the disabled skill's
    /// content inside) strikes only the skill content: the bullet and the
    /// `Context:` and `builtin` rows stay unstruck. This locks the
    /// bullet-not-struck contract against a real context row, not a synthetic
    /// marker.
    #[test]
    fn context_notice_strikes_skill_content_not_the_bullet() {
        let t = transcript_with(EntryKind::Notice(NoticeEntry {
            level: NoticeLevel::Info,
            text: "Context:\n  - builtin (system prompt)\n  - \
                   \x1b[9m~/x/SKILL.md (skill: y, disabled)\x1b[29m"
                .into(),
            entry: None,
        }));
        let spans = entry_spans(&t.entries()[0], &styles());
        let struck = spans
            .iter()
            .find(|s| s.style.strikethrough)
            .expect("the disabled skill row is struck");
        assert!(
            struck.text.contains("disabled") && struck.text.contains("SKILL.md"),
            "the struck span carries the skill content and path: {struck:?}"
        );
        for s in &spans {
            if s.text.contains("  - ") || s.text.contains("Context:") || s.text.contains("builtin")
            {
                assert!(
                    !s.style.strikethrough,
                    "the bullet, header, and builtin rows stay unstruck: {s:?}"
                );
            }
        }
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
            entry: None,
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

    /// A fresh short transcript sits at the bottom of the chat slot, so the
    /// first entry lands just above the editor rather than at row 0. Under
    /// top-anchoring the content would start at row 0, which is the mutation
    /// this guards against.
    #[test]
    fn short_transcript_bottom_anchors_the_first_entry() {
        let chat = chat_with_notices(1);
        let mut view = transcript_view(&chat);

        let surface = view.draw(&draw_ctx(40, 10));
        let rows = crate::test_support::rows(&surface);
        assert_eq!(rows.len(), 10, "{rows:?}");

        // The single notice is short, so the top of the slot is blank and the
        // entry sits at the bottom.
        assert_eq!(
            rows[0], "",
            "top row must be blank, not the entry: {rows:?}"
        );
        let content_row = rows
            .iter()
            .position(|r| r.contains("row 0"))
            .expect("the notice renders somewhere");
        assert!(
            content_row >= 8,
            "the first entry sits near the bottom, at row {content_row}: {rows:?}"
        );
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
                thinking_display: "default".into(),
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
                None,
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
            Rc::new(RefCell::new(None)),
            Rc::new(std::cell::Cell::new(None)),
            Rc::new(RefCell::new(ImageStore::default())),
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
        mouse_with_mods(col, row, kind, mouse::Modifiers::empty())
    }

    fn mouse_with_mods(col: i16, row: i16, kind: mouse::Type, mods: mouse::Modifiers) -> Event {
        Event::Mouse(mouse::Mouse {
            col,
            row,
            xoffset: 0,
            yoffset: 0,
            button: mouse::Button::Left,
            mods,
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

        // The bar column hit-tests to the bars directly, so here we drive the
        // transcript's forwarding path via handle_event, where the bars grab
        // the press.
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
        view.page_up(&mut EventContext::new());
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
            view.page_down(&mut EventContext::new());
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
        view.scroll_to_top(&mut EventContext::new());
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
        view.scroll_to_bottom(&mut EventContext::new());
        assert!(view.follow_tail, "End re-engages follow-tail");
        let rows = crate::test_support::rows(&view.draw(&ctx));
        assert!(view.list.borrow().is_at_bottom(), "back at the bottom");
        assert!(
            rows.join("\n").contains("row 49"),
            "shows the last row: {rows:?}"
        );
    }

    /// Resuming follow-tail is the host-facing counterpart to editor-mode End:
    /// it cancels stale motion and pins the next draw to the live bottom.
    #[test]
    fn resume_follow_tail_cancels_motion_and_lands_at_the_bottom() {
        let chat = chat_with_notices(50);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let _ = view.draw(&ctx);
        view.scroll_to_top(&mut EventContext::new());
        let _ = view.draw(&ctx);
        assert!(
            !view.list.borrow().is_at_bottom(),
            "starts away from the tail"
        );

        view.scroll_anim = Some(ScrollAnim {
            total: 20.0,
            applied: 0.0,
            completion: ScrollCompletion::Page,
            start: Instant::now(),
            duration: Duration::from_millis(100),
        });
        view.resume_follow_tail();
        assert!(view.follow_tail, "follow-tail re-engaged");
        assert!(view.scroll_anim.is_none(), "stale glide cancelled");

        let _ = view.draw(&ctx);
        assert!(view.list.borrow().is_at_bottom(), "next draw pins the tail");
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
        view.scroll_to_top(&mut EventContext::new());
        assert_eq!(
            view.list.borrow().cursor,
            0,
            "Home moves to the first user message"
        );
        assert!(!view.follow_tail, "focus mode keeps follow-tail disengaged");

        // End moves the cursor back to the last user message.
        view.scroll_to_bottom(&mut EventContext::new());
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
        view.page_up(&mut EventContext::new());
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

    /// `b` is inert in a sub-agent view: `focused_message_id` returns `None`
    /// even with the cursor on a sub-agent user row, so no branch anchor can be
    /// armed there. Anchoring the user-thread head at a sub-agent user message
    /// would splice the main conversation onto a sub thread, the data-
    /// corruption path the Main-view gate guards. `focused_message_text` still
    /// resolves, proving the `None` comes from the view gate and not from an
    /// empty or non-user cursor.
    #[test]
    fn focused_message_id_is_none_in_a_sub_agent_view() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        // A real Main branch point, so the None below is the sub-view gate, not
        // an empty session.
        apply(&chat, &mut life, user_end("main message"));
        apply(
            &chat,
            &mut life,
            assistant_message_end(text_message("main reply")),
        );
        // A sub-agent whose task prompt lands as a user row.
        spawn_sub(&chat, &mut life);
        apply(
            &chat,
            &mut life,
            AgentEvent::MessageEnd {
                agent_id: AgentId::Sub(0),
                message: AgentMessage::wire(Message::User(aj_models::types::UserMessage::text(
                    "sub task prompt",
                ))),
            },
        );

        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let _ = view.draw(&ctx);
        // Switch to the sub view and focus its last user row, as the host does.
        chat.borrow_mut().set_active_view(AgentId::Sub(0));
        view.reset_to_tail();
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &Event::FocusIn);
        let _ = view.draw(&ctx);

        assert!(view.in_focus_mode(), "the sub view is in focus mode");
        assert_eq!(
            view.focused_message_text().as_deref(),
            Some("sub task prompt"),
            "the cursor sits on the sub-agent user row"
        );
        assert!(
            view.focused_message_id().is_none(),
            "the branch anchor is inert in a sub-agent view"
        );
    }
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

    /// Esc out of focus mode lands the viewport at the live bottom: it
    /// re-engages follow-tail, and the exit's `FocusOut` then lets the next
    /// draw pin the viewport there.
    #[test]
    fn esc_exit_lands_at_the_bottom() {
        let chat = chat_with_user_messages(6);
        let mut view = transcript_view(&chat);
        view.set_on_exit_focus(Box::new(|_ctx| {}));
        let ctx = draw_ctx(40, 8);
        let _ = view.draw(&ctx);

        // Enter focus and scroll up to the first user message.
        view.handle_event(&mut EventContext::new(), &Event::FocusIn);
        let _ = view.draw(&ctx);
        view.scroll_to_top(&mut EventContext::new());
        let _ = view.draw(&ctx);
        assert!(!view.follow_tail, "focus mode keeps follow-tail off");
        assert!(
            !view.list.borrow().is_at_bottom(),
            "scrolled up off the bottom"
        );

        // A glide is in flight (simulated: this non-Rc view can't arm one
        // itself), so Esc must cancel it as well as re-engaging follow-tail.
        view.scroll_anim = Some(ScrollAnim {
            total: -20.0,
            applied: 0.0,
            completion: ScrollCompletion::Page,
            start: Instant::now(),
            duration: Duration::from_millis(100),
        });

        // Esc re-engages follow-tail and exits. The exit's FocusOut clears
        // focus mode, and the next draw pins the viewport to the bottom.
        view.handle_event(
            &mut EventContext::new(),
            &key_press(Key::ESCAPE, Modifiers::empty()),
        );
        assert!(view.follow_tail, "Esc re-engages follow-tail");
        assert!(
            view.scroll_anim.is_none(),
            "Esc cancelled the in-flight glide"
        );
        view.handle_event(&mut EventContext::new(), &Event::FocusOut);
        assert!(!view.in_focus_mode(), "exited focus mode");
        let _ = view.draw(&ctx);
        assert!(
            view.list.borrow().is_at_bottom(),
            "rests at the live bottom"
        );
    }

    /// Losing focus without Esc (an overlay steals it, firing a bare
    /// `FocusOut`) must NOT jump to the bottom: only Esc re-engages follow-tail,
    /// so a scrolled-up transcript keeps its position under an overlay.
    #[test]
    fn overlay_steal_preserves_the_scroll_position() {
        let chat = chat_with_user_messages(6);
        let mut view = transcript_view(&chat);
        view.set_on_exit_focus(Box::new(|_ctx| {}));
        let ctx = draw_ctx(40, 8);
        let _ = view.draw(&ctx);

        view.handle_event(&mut EventContext::new(), &Event::FocusIn);
        let _ = view.draw(&ctx);
        view.scroll_to_top(&mut EventContext::new());
        let _ = view.draw(&ctx);
        assert!(
            !view.list.borrow().is_at_bottom(),
            "scrolled up off the bottom"
        );

        // A bare FocusOut (not the Esc chord) exits the mode but leaves
        // follow-tail off, so the next draw keeps the scrolled-up position.
        view.handle_event(&mut EventContext::new(), &Event::FocusOut);
        assert!(!view.in_focus_mode(), "exited focus mode");
        assert!(
            !view.follow_tail,
            "an overlay steal does not re-engage the tail"
        );
        let _ = view.draw(&ctx);
        assert!(
            !view.list.borrow().is_at_bottom(),
            "scroll position preserved under the overlay"
        );
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
        view.scroll_to_top(&mut EventContext::new());
        assert_eq!(view.list.borrow().cursor, 0, "scroll_to_top stays at 0");
        view.scroll_to_bottom(&mut EventContext::new());
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

    /// Transcript-focus navigation steps between user prompts only, so a
    /// task notification is never a focus stop (Spec E section 1). The
    /// notice sits between two real prompts and must be skipped.
    #[test]
    fn transcript_focus_skips_task_notifications() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(&chat, &mut life, user_end("first prompt"));
        apply(
            &chat,
            &mut life,
            task_notification_end("sleep", TaskOutcome::Succeeded, "done"),
        );
        apply(&chat, &mut life, user_end("second prompt"));
        let view = transcript_view(&chat);
        // Entries: user(0), notification(1), user(2). Only the two user
        // prompts are focus stops; the notice at index 1 is skipped.
        assert_eq!(view.user_message_indices(), vec![0, 2]);
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

        view.focus_prev_user_message(&mut EventContext::new());
        assert_eq!(view.list.borrow().cursor, 0, "nothing older to step to");
        view.focus_next_user_message(&mut EventContext::new());
        assert_eq!(view.list.borrow().cursor, 0, "nothing newer to step to");
    }

    /// A focus step that must move the viewport more than a line glides there
    /// over several frames instead of snapping: the cursor lands on the target
    /// at once, the viewport lags, and driving the tick chain to completion
    /// leaves it exactly where the instant snap would have (a self-ref-less
    /// reference view that snaps is the oracle).
    #[test]
    fn focus_scroll_glides_to_the_snap_destination() {
        let chat = chat_with_user_messages(6);
        let ctx = draw_ctx(40, 6);

        // Oracle: a view with no self-reference, so its focus steps snap.
        let mut oracle = transcript_view(&chat);
        let _ = oracle.draw(&ctx);
        oracle.handle_event(&mut EventContext::new(), &Event::FocusIn);
        let _ = oracle.draw(&ctx);
        oracle.scroll_to_top(&mut EventContext::new());
        let oracle_rows = crate::test_support::rows(&oracle.draw(&ctx));
        let oracle_top = oracle.list.borrow().scroll_top();
        let oracle_off = oracle.list.borrow().scroll_offset();
        assert_eq!(
            oracle.list.borrow().cursor,
            0,
            "snapped onto the first message"
        );

        // Subject: identical steps, but behind an `Rc` with a self-reference,
        // so the jump to the top animates.
        let view = Rc::new(RefCell::new(transcript_view(&chat)));
        view.borrow_mut().set_widget_ref(Rc::downgrade(&view));
        let _ = view.borrow_mut().draw(&ctx);
        view.borrow_mut()
            .handle_event(&mut EventContext::new(), &Event::FocusIn);
        let _ = view.borrow_mut().draw(&ctx);
        settle_scroll_anim(&view, &ctx); // drain any enter-focus glide first
        let before_top = view.borrow().list.borrow().scroll_top();

        view.borrow_mut().scroll_to_top(&mut EventContext::new());
        assert!(
            view.borrow().scroll_anim.is_some(),
            "a multi-message jump up animates rather than snapping"
        );
        assert_eq!(
            view.borrow().list.borrow().cursor,
            0,
            "the cursor lands on the target immediately"
        );
        assert_eq!(
            view.borrow().list.borrow().scroll_top(),
            before_top,
            "the viewport has not jumped yet"
        );

        settle_scroll_anim(&view, &ctx);
        let rows = crate::test_support::rows(&view.borrow_mut().draw(&ctx));
        assert_eq!(
            view.borrow().list.borrow().scroll_top(),
            oracle_top,
            "the glide lands on the snap's top item"
        );
        assert_eq!(
            view.borrow().list.borrow().scroll_offset(),
            oracle_off,
            "the glide lands on the snap's line offset"
        );
        assert_eq!(
            rows, oracle_rows,
            "the glide's final frame matches the snap"
        );
    }

    /// Advance through deterministic animation frames and redraw. Intermediate
    /// draws measure newly visible items before the final focus snap.
    fn settle_scroll_anim(view: &Rc<RefCell<TranscriptView>>, ctx: &DrawContext) {
        let Some((start, duration)) = ({
            let view = view.borrow();
            view.scroll_anim
                .as_ref()
                .map(|anim| (anim.start, anim.duration))
        }) else {
            return;
        };
        for frame in 1..=10 {
            let now = start + duration.mul_f64(f64::from(frame) / 10.0);
            view.borrow_mut()
                .advance_scroll_anim_at(&mut EventContext::new(), now);
            let _ = view.borrow_mut().draw(ctx);
        }
    }

    /// Editor-mode page scrolling glides the viewport instead of snapping, and
    /// the glide lands where the instant page scroll would (a self-ref-less
    /// reference view that snaps is the oracle).
    #[test]
    fn page_scroll_glides_to_the_snap_destination() {
        let chat = chat_with_notices(50);
        let ctx = draw_ctx(40, 10);

        // Oracle: no self-reference, so page_up snaps.
        let mut oracle = transcript_view(&chat);
        let _ = oracle.draw(&ctx);
        oracle.page_up(&mut EventContext::new());
        let oracle_rows = crate::test_support::rows(&oracle.draw(&ctx));
        let oracle_top = oracle.list.borrow().scroll_top();
        let oracle_off = oracle.list.borrow().scroll_offset();

        // Subject: identical page_up, but with a self-reference so it glides.
        let view = Rc::new(RefCell::new(transcript_view(&chat)));
        view.borrow_mut().set_widget_ref(Rc::downgrade(&view));
        let _ = view.borrow_mut().draw(&ctx);
        let before_top = view.borrow().list.borrow().scroll_top();

        view.borrow_mut().page_up(&mut EventContext::new());
        assert!(
            view.borrow().scroll_anim.is_some(),
            "page up off the bottom animates"
        );
        assert!(!view.borrow().follow_tail, "page up disengages follow-tail");
        assert_eq!(
            view.borrow().list.borrow().scroll_top(),
            before_top,
            "the viewport has not jumped yet"
        );

        settle_scroll_anim(&view, &ctx);
        let rows = crate::test_support::rows(&view.borrow_mut().draw(&ctx));
        assert_eq!(view.borrow().list.borrow().scroll_top(), oracle_top);
        assert_eq!(view.borrow().list.borrow().scroll_offset(), oracle_off);
        assert_eq!(
            rows, oracle_rows,
            "the glide's final frame matches the snap"
        );
    }

    /// A page glide is inert at the edge it heads for: paging down at the
    /// bottom and up at the top start no animation.
    #[test]
    fn page_scroll_is_inert_at_the_edge() {
        let chat = chat_with_notices(50);
        let ctx = draw_ctx(40, 10);
        let view = Rc::new(RefCell::new(transcript_view(&chat)));
        view.borrow_mut().set_widget_ref(Rc::downgrade(&view));
        let _ = view.borrow_mut().draw(&ctx);

        // Opens at the bottom, so page down cannot move.
        assert!(view.borrow().list.borrow().is_at_bottom());
        view.borrow_mut().page_down(&mut EventContext::new());
        assert!(
            view.borrow().scroll_anim.is_none(),
            "page down at the bottom is inert"
        );

        // Jump to the top (editor-mode Home snaps), then page up cannot move.
        view.borrow_mut().scroll_to_top(&mut EventContext::new());
        let _ = view.borrow_mut().draw(&ctx);
        assert_eq!(view.borrow().list.borrow().scroll_top(), 0);
        view.borrow_mut().page_up(&mut EventContext::new());
        assert!(
            view.borrow().scroll_anim.is_none(),
            "page up at the top is inert"
        );
    }

    /// A page glide moves the viewport gradually: one frame in, the top line
    /// sits strictly between the start and the settled destination, so the
    /// glide is not a disguised instant jump.
    #[test]
    fn page_glide_moves_gradually() {
        let chat = chat_with_notices(50);
        let ctx = draw_ctx(40, 10);
        let view = Rc::new(RefCell::new(transcript_view(&chat)));
        view.borrow_mut().set_widget_ref(Rc::downgrade(&view));
        let _ = view.borrow_mut().draw(&ctx);

        let before = top_line(&view);
        view.borrow_mut().page_up(&mut EventContext::new());

        // Halfway through: the viewport has moved up, but not all the way.
        let halfway = {
            let view = view.borrow();
            let anim = view.scroll_anim.as_ref().expect("animation in flight");
            anim.start + anim.duration / 2
        };
        view.borrow_mut()
            .advance_scroll_anim_at(&mut EventContext::new(), halfway);
        let _ = view.borrow_mut().draw(&ctx);
        let mid = top_line(&view);

        settle_scroll_anim(&view, &ctx);
        let _ = view.borrow_mut().draw(&ctx);
        let end = top_line(&view);

        assert!(end < before, "page up moved the viewport toward the top");
        assert!(
            end < mid && mid < before,
            "one frame lands strictly between start ({before}) and end ({end}): mid={mid}"
        );
    }

    /// A second page press while a page glide is in flight carries the
    /// unfinished travel, so two rapid presses reach the same cumulative
    /// position two instant page scrolls would (an oracle proves the target).
    #[test]
    fn page_glide_retarget_reaches_the_cumulative_destination() {
        let chat = chat_with_notices(50);
        let ctx = draw_ctx(40, 10);

        // Oracle: two instant page scrolls up from the bottom. `page_up`
        // (not raw `scroll_lines`) disengages follow-tail, else the draw
        // re-pins the viewport to the bottom.
        let mut oracle = transcript_view(&chat);
        let _ = oracle.draw(&ctx);
        oracle.page_up(&mut EventContext::new());
        oracle.page_up(&mut EventContext::new());
        let _ = oracle.draw(&ctx);
        let oracle_line = top_line_view(&oracle);

        // Subject: two page_up presses with no tick between them, so the second
        // carries the first glide's full (un-applied) travel.
        let view = Rc::new(RefCell::new(transcript_view(&chat)));
        view.borrow_mut().set_widget_ref(Rc::downgrade(&view));
        let _ = view.borrow_mut().draw(&ctx);
        view.borrow_mut().page_up(&mut EventContext::new());
        view.borrow_mut().page_up(&mut EventContext::new());
        settle_scroll_anim(&view, &ctx);
        let _ = view.borrow_mut().draw(&ctx);

        assert_eq!(
            top_line(&view),
            oracle_line,
            "two rapid page ups land where two instant page ups would"
        );
    }

    /// Paging back down with the glide re-engages follow-tail once the viewport
    /// reaches the bottom, so streaming resumes (the instant-path counterpart is
    /// `page_up_disengages_and_page_down_reengages_follow_tail`).
    #[test]
    fn page_glide_down_reengages_follow_tail() {
        let chat = chat_with_notices(50);
        let ctx = draw_ctx(40, 10);
        let view = Rc::new(RefCell::new(transcript_view(&chat)));
        view.borrow_mut().set_widget_ref(Rc::downgrade(&view));
        let _ = view.borrow_mut().draw(&ctx);

        view.borrow_mut().page_up(&mut EventContext::new());
        settle_scroll_anim(&view, &ctx);
        assert!(!view.borrow().follow_tail, "paged up, follow-tail off");

        // Page down (gliding) until follow-tail re-engages at the bottom.
        for _ in 0..8 {
            view.borrow_mut().page_down(&mut EventContext::new());
            settle_scroll_anim(&view, &ctx);
            let _ = view.borrow_mut().draw(&ctx);
            if view.borrow().follow_tail {
                break;
            }
        }
        assert!(
            view.borrow().follow_tail,
            "gliding down to the bottom re-engages follow-tail"
        );
        assert!(view.borrow().list.borrow().is_at_bottom());
    }

    /// `at_scroll_end` reports the edge for the direction it is asked about,
    /// not the other one (guards the direction sign the early-stop relies on).
    #[test]
    fn at_scroll_end_reports_the_edge_for_each_direction() {
        let chat = chat_with_notices(50);
        let ctx = draw_ctx(40, 10);
        let mut view = transcript_view(&chat);
        let _ = view.draw(&ctx);

        // At the bottom: heading down is at the end, heading up is not.
        assert!(view.at_scroll_end(false), "at the bottom heading down");
        assert!(!view.at_scroll_end(true), "at the bottom, not the top");

        // At the top: the reverse.
        view.scroll_to_top(&mut EventContext::new());
        let _ = view.draw(&ctx);
        assert!(view.at_scroll_end(true), "at the top heading up");
        assert!(!view.at_scroll_end(false), "at the top, not the bottom");
    }

    /// A focus step that resolves to a snap cancels an in-flight page glide, so
    /// the glide cannot keep driving the viewport away from the focused
    /// message (regression: the snap path used to leave the glide running).
    #[test]
    fn focus_snap_cancels_an_in_flight_page_glide() {
        let chat = chat_with_user_messages(6);
        let ctx = draw_ctx(40, 8);
        let view = Rc::new(RefCell::new(transcript_view(&chat)));
        view.borrow_mut().set_widget_ref(Rc::downgrade(&view));
        let _ = view.borrow_mut().draw(&ctx);

        // Enter focus and settle onto the last user message.
        view.borrow_mut()
            .handle_event(&mut EventContext::new(), &Event::FocusIn);
        let _ = view.borrow_mut().draw(&ctx);
        settle_scroll_anim(&view, &ctx);
        assert!(
            view.borrow().scroll_anim.is_none(),
            "settled after entering"
        );
        let cursor = view.borrow().list.borrow().cursor;

        // Page up starts a glide but the viewport has not moved yet (no tick).
        view.borrow_mut().page_up(&mut EventContext::new());
        assert!(view.borrow().scroll_anim.is_some(), "page up armed a glide");

        // End re-focuses the last message, still at rest, so it snaps. The snap
        // must cancel the page glide rather than let it scroll on.
        view.borrow_mut().scroll_to_bottom(&mut EventContext::new());
        assert!(
            view.borrow().scroll_anim.is_none(),
            "the focus snap cancelled the in-flight page glide"
        );
        assert_eq!(
            view.borrow().list.borrow().cursor,
            cursor,
            "still on the last user message"
        );
    }

    /// An instant editor-mode scroll (Home) cancels an in-flight page glide, so
    /// the glide cannot drive the viewport back off the jumped-to position
    /// (regression: a page-down glide plus Home used to drift down from the
    /// top).
    #[test]
    fn instant_scroll_cancels_an_in_flight_page_glide() {
        let chat = chat_with_notices(50);
        let ctx = draw_ctx(40, 10);
        let view = Rc::new(RefCell::new(transcript_view(&chat)));
        view.borrow_mut().set_widget_ref(Rc::downgrade(&view));
        let _ = view.borrow_mut().draw(&ctx);

        // Get off the bottom so a page-down glide has somewhere to go.
        view.borrow_mut().page_up(&mut EventContext::new());
        settle_scroll_anim(&view, &ctx);

        // Arm a page-down glide (viewport not moved yet), then jump to the top.
        view.borrow_mut().page_down(&mut EventContext::new());
        assert!(
            view.borrow().scroll_anim.is_some(),
            "page down armed a glide"
        );
        view.borrow_mut().scroll_to_top(&mut EventContext::new());
        assert!(
            view.borrow().scroll_anim.is_none(),
            "Home cancelled the in-flight page glide"
        );

        // Any orphaned tick must not drift the viewport back off the top.
        for _ in 0..4 {
            view.borrow_mut()
                .handle_event(&mut EventContext::new(), &Event::Tick);
            let _ = view.borrow_mut().draw(&ctx);
        }
        assert_eq!(
            view.borrow().list.borrow().scroll_top(),
            0,
            "stays pinned at the top"
        );
        assert_eq!(view.borrow().list.borrow().scroll_offset(), 0);
    }

    /// Absolute top line of a `ListView` (top item's start line plus the
    /// in-item offset), the linear measure the glide tests compare.
    fn top_line(view: &Rc<RefCell<TranscriptView>>) -> i64 {
        top_line_view(&view.borrow())
    }

    fn top_line_view(view: &TranscriptView) -> i64 {
        let list = view.list.borrow();
        let top = usize::try_from(list.scroll_top()).expect("top fits usize");
        i64::try_from(list.item_top_line(top)).expect("line fits i64")
            + i64::from(list.scroll_offset())
    }

    /// The number of top-left corner glyphs in a drawn view: one per bordered
    /// (focused) bubble.
    fn border_count(view: &mut TranscriptView, ctx: &DrawContext) -> usize {
        crate::test_support::rows(&view.draw(ctx))
            .join("\n")
            .matches('\u{250f}')
            .count()
    }

    /// Screen rows of the focused bubble's top-left (`┏`) and bottom-left (`┗`)
    /// border corners, the frame that marks the focused message.
    fn focus_border_rows(view: &Rc<RefCell<TranscriptView>>, ctx: &DrawContext) -> (usize, usize) {
        let rows = crate::test_support::rows(&view.borrow_mut().draw(ctx));
        let top = rows
            .iter()
            .position(|r| r.contains('\u{250f}'))
            .expect("a focused bubble draws a top border");
        let bottom = rows
            .iter()
            .position(|r| r.contains('\u{2517}'))
            .expect("a focused bubble draws a bottom border");
        (top, bottom)
    }

    /// Stepping up to an older message leaves `FOCUS_SCROLL_MARGIN` rows of the
    /// preceding reply above it, so the focused message never sits flush against
    /// the top edge.
    #[test]
    fn focus_step_up_leaves_context_above_the_message() {
        // Users at 0, 2, ..., 14 with replies between, over a viewport far
        // shorter than the transcript so stepping must scroll.
        let chat = chat_with_user_messages(8);
        let ctx = draw_ctx(40, 10);
        let view = Rc::new(RefCell::new(transcript_view(&chat)));
        view.borrow_mut().set_widget_ref(Rc::downgrade(&view));
        let _ = view.borrow_mut().draw(&ctx);
        view.borrow_mut()
            .handle_event(&mut EventContext::new(), &Event::FocusIn);
        let _ = view.borrow_mut().draw(&ctx);
        settle_scroll_anim(&view, &ctx);

        // Step up to a middle message (index 8), with content on both sides.
        while view.borrow().list.borrow().cursor > 8 {
            view.borrow_mut()
                .focus_prev_user_message(&mut EventContext::new());
            settle_scroll_anim(&view, &ctx);
        }
        assert_eq!(view.borrow().list.borrow().cursor, 8);

        let (top, _) = focus_border_rows(&view, &ctx);
        assert_eq!(
            top,
            usize::from(FOCUS_SCROLL_MARGIN),
            "the message's top border rests FOCUS_SCROLL_MARGIN rows down",
        );
        // The rows above the border show the preceding reply, not blank filler.
        let rows = crate::test_support::rows(&view.borrow_mut().draw(&ctx));
        assert!(
            rows[..top].iter().any(|r| r.contains("assistant")),
            "context above the focused message: {rows:?}",
        );
    }

    /// Stepping down to a newer message below the fold leaves
    /// `FOCUS_SCROLL_MARGIN` rows of the following reply below it, so the
    /// focused message never sits flush against the bottom edge.
    #[test]
    fn focus_step_down_leaves_context_below_the_message() {
        let chat = chat_with_user_messages(8);
        let ctx = draw_ctx(40, 10);
        let view = Rc::new(RefCell::new(transcript_view(&chat)));
        view.borrow_mut().set_widget_ref(Rc::downgrade(&view));
        let _ = view.borrow_mut().draw(&ctx);
        view.borrow_mut()
            .handle_event(&mut EventContext::new(), &Event::FocusIn);
        let _ = view.borrow_mut().draw(&ctx);
        settle_scroll_anim(&view, &ctx);

        // From the top, step down one message: the target sits below the fold,
        // so it rests near the bottom with context beneath it.
        view.borrow_mut().scroll_to_top(&mut EventContext::new());
        settle_scroll_anim(&view, &ctx);
        view.borrow_mut()
            .focus_next_user_message(&mut EventContext::new());
        settle_scroll_anim(&view, &ctx);
        assert_eq!(view.borrow().list.borrow().cursor, 2);

        let vh = 10usize;
        let (_, bottom) = focus_border_rows(&view, &ctx);
        // The bubble is `┏`, body, `┗`, then one trailing spacer row. The
        // spacer plus FOCUS_SCROLL_MARGIN rows of the following reply sit below
        // the bottom border.
        assert_eq!(
            vh - 1 - bottom,
            usize::from(FOCUS_SCROLL_MARGIN) + 1,
            "the message's bottom leaves FOCUS_SCROLL_MARGIN rows below it",
        );
        let rows = crate::test_support::rows(&view.borrow_mut().draw(&ctx));
        assert!(
            rows[bottom + 1..].iter().any(|r| r.contains("assistant")),
            "context below the focused message: {rows:?}",
        );
    }

    /// A message taller than the viewport-minus-margin still lands fully in
    /// view when jumped to from afar: the margin is given up rather than
    /// pushing the message's bottom off-screen. The jump target is off-screen
    /// and unmeasured at planning time, so its height estimate is small and the
    /// snap must re-measure it before landing.
    #[test]
    fn focus_jump_to_a_tall_message_keeps_it_fully_visible() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        // Two notices precede the first user message, so it is not entry 0 and
        // has content above it (a top margin is allowed).
        apply(
            &chat,
            &mut life,
            AgentEvent::Notice {
                agent_id: AgentId::Main,
                text: "note A".into(),
            },
        );
        apply(
            &chat,
            &mut life,
            AgentEvent::Notice {
                agent_id: AgentId::Main,
                text: "note B".into(),
            },
        );
        // A six-line first user message (entry index 2): taller than
        // `vh - margin`, still short enough to fit the viewport.
        let tall = (0..6)
            .map(|i| format!("tall line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        apply(&chat, &mut life, user_end(&tall));
        apply(
            &chat,
            &mut life,
            assistant_message_end(text_message("reply 0")),
        );
        for i in 1..9 {
            apply(&chat, &mut life, user_end(&format!("short {i}")));
            apply(
                &chat,
                &mut life,
                assistant_message_end(text_message(&format!("reply {i}"))),
            );
        }

        let ctx = draw_ctx(48, 10);
        let view = Rc::new(RefCell::new(transcript_view(&chat)));
        view.borrow_mut().set_widget_ref(Rc::downgrade(&view));
        let _ = view.borrow_mut().draw(&ctx);
        view.borrow_mut()
            .handle_event(&mut EventContext::new(), &Event::FocusIn);
        let _ = view.borrow_mut().draw(&ctx);
        settle_scroll_anim(&view, &ctx);

        // Home (focus mode) jumps to the tall first user message, which is
        // off-screen and unmeasured until the glide brings it into view.
        view.borrow_mut().scroll_to_top(&mut EventContext::new());
        settle_scroll_anim(&view, &ctx);
        assert_eq!(view.borrow().list.borrow().cursor, 2);

        // Both borders are on screen (focus_border_rows panics otherwise) and
        // the message's last line shows: it is framed whole, not clipped.
        let (_, bottom) = focus_border_rows(&view, &ctx);
        assert!(bottom < 10, "the bottom border is on screen: {bottom}");
        let rows = crate::test_support::rows(&view.borrow_mut().draw(&ctx));
        assert!(
            rows.iter().any(|r| r.contains("tall line 5")),
            "the last content line is visible: {rows:?}",
        );
    }

    /// The last user message has no reply of its own beyond the fold, so
    /// stepping to it hugs the bottom rather than baring blank rows the margin
    /// would otherwise reserve.
    #[test]
    fn focus_last_message_hugs_the_bottom() {
        // A transcript whose final entry is a user message (no trailing reply),
        // so there is nothing below it to fill a bottom margin.
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        for i in 0..8 {
            apply(&chat, &mut life, user_end(&format!("user {i}")));
            apply(
                &chat,
                &mut life,
                assistant_message_end(text_message(&format!("assistant {i}"))),
            );
        }
        apply(&chat, &mut life, user_end("last prompt"));

        let ctx = draw_ctx(40, 10);
        let view = Rc::new(RefCell::new(transcript_view(&chat)));
        view.borrow_mut().set_widget_ref(Rc::downgrade(&view));
        let _ = view.borrow_mut().draw(&ctx);
        view.borrow_mut()
            .handle_event(&mut EventContext::new(), &Event::FocusIn);
        let _ = view.borrow_mut().draw(&ctx);
        settle_scroll_anim(&view, &ctx);

        // Scroll up, then step back down to the last message.
        view.borrow_mut().scroll_to_top(&mut EventContext::new());
        settle_scroll_anim(&view, &ctx);
        view.borrow_mut().scroll_to_bottom(&mut EventContext::new());
        settle_scroll_anim(&view, &ctx);

        // Only the bubble's trailing spacer sits below its bottom border: no
        // FOCUS_SCROLL_MARGIN rows are reserved, since there is nothing beyond
        // the last message to show there.
        let vh = 10usize;
        let (_, bottom) = focus_border_rows(&view, &ctx);
        assert_eq!(
            vh - 1 - bottom,
            1,
            "the last message hugs the bottom, no reserved margin below it",
        );
        let rows = crate::test_support::rows(&view.borrow_mut().draw(&ctx));
        assert!(
            rows.iter().any(|r| r.contains("last prompt")),
            "the last message is on screen: {rows:?}",
        );
    }

    /// While a branch is armed the highlight box stays on the branched-from
    /// message even though focus mode is off (the editor holds focus), and its
    /// bottom edge shows the branching hint rather than the copy / branch keys.
    /// Clearing the anchor drops the box.
    #[test]
    fn armed_branch_border_marks_the_message_without_focus_mode() {
        // Users at 0, 2, 4 ("user 0", "user 1", "user 2"); the middle one is
        // the branch target.
        let chat = chat_with_user_messages(3);
        let armed_id = {
            let chat = chat.borrow();
            match &chat.transcript(AgentId::Main).unwrap().entries()[2].kind {
                EntryKind::User(u) => u.message_id.clone(),
                _ => panic!("entry 2 is a user message"),
            }
        };
        let branch_armed = Rc::new(RefCell::new(armed_id));
        let theme = Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor);
        // Tall viewport so the whole transcript fits and the assertions are
        // exact.
        let mut view = TranscriptView::new(
            Rc::clone(&chat),
            &theme,
            Rc::new(std::cell::Cell::new(false)),
            Rc::clone(&branch_armed),
            Rc::new(std::cell::Cell::new(None)),
            Rc::new(RefCell::new(ImageStore::default())),
        );
        let ctx = draw_ctx(48, 40);
        let rows = crate::test_support::rows(&view.draw(&ctx));
        let joined = rows.join("\n");

        assert!(!view.in_focus_mode(), "focus mode is off while branching");
        assert_eq!(
            joined.matches('\u{250f}').count(),
            1,
            "exactly the armed message is bordered: {rows:?}",
        );
        // The bordered bubble is the branch target, and its edge carries the
        // branch hint, not the focus copy / branch hint.
        let top = rows
            .iter()
            .position(|r| r.contains('\u{250f}'))
            .expect("a bordered bubble");
        assert!(
            rows[top + 1].contains("user 1"),
            "bordered message: {rows:?}"
        );
        assert!(
            joined.contains("branching") && joined.contains("Esc to cancel"),
            "branch hint on the border edge: {rows:?}",
        );
        assert!(
            !joined.contains("to copy") && !joined.contains("to branch"),
            "the focus hint is replaced while branching: {rows:?}",
        );

        // Clearing the anchor drops the box.
        *branch_armed.borrow_mut() = None;
        let rows = crate::test_support::rows(&view.draw(&ctx));
        assert_eq!(
            rows.join("\n").matches('\u{250f}').count(),
            0,
            "the box drops when the branch is disarmed: {rows:?}",
        );
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
            thinking_display: "default".into(),
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
        let _ = reduce(&mut chat.borrow_mut(), life, event, None);
    }

    /// A caching builder over `chat` with a fresh cache and a concrete styles
    /// instance the uncached reference reuses, so cached and uncached renders
    /// are byte-comparable.
    fn caching_builder(chat: &Rc<RefCell<ChatState>>) -> EntryBuilder {
        let styles = Rc::new(styles());
        let copy_label = Rc::new(copy_label_spans(&styles));
        let branch_label = Rc::new(branch_label_spans(&styles));
        EntryBuilder {
            chat: Rc::clone(chat),
            styles,
            cache: Rc::new(RefCell::new(EntryRenderCache::new())),
            focus_mode: Rc::new(std::cell::Cell::new(false)),
            branch_armed: Rc::new(RefCell::new(None)),
            copy_label,
            branch_label,
            image_store: Rc::new(RefCell::new(ImageStore::default())),
        }
    }

    /// A caching builder with images enabled, sharing `store`, so a test can
    /// observe the pending recording and the transmit-driven fingerprint flip.
    fn image_builder(
        chat: &Rc<RefCell<ChatState>>,
        store: &Rc<RefCell<ImageStore>>,
    ) -> EntryBuilder {
        let styles = Rc::new(TranscriptStyles::from_theme(
            &Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor),
            TerminalCaps {
                images: true,
                ..TerminalCaps::default()
            },
        ));
        let copy_label = Rc::new(copy_label_spans(&styles));
        let branch_label = Rc::new(branch_label_spans(&styles));
        EntryBuilder {
            chat: Rc::clone(chat),
            styles,
            cache: Rc::new(RefCell::new(EntryRenderCache::new())),
            focus_mode: Rc::new(std::cell::Cell::new(false)),
            branch_armed: Rc::new(RefCell::new(None)),
            copy_label,
            branch_label,
            image_store: Rc::clone(store),
        }
    }

    /// A chat with a single tool-result image entry on Main.
    fn chat_with_image_entry() -> Rc<RefCell<ChatState>> {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
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
                ToolDetails::Image {
                    summary: "/tmp/pic.png".into(),
                    mime_type: "image/png".into(),
                    original_dimensions: (100, 80),
                    displayed_dimensions: (100, 80),
                },
            ),
        );
        chat
    }

    /// A visible untransmitted image records a pending key and draws the blank
    /// reserve; once the id lands the fingerprint flips, so the rebuild places
    /// the image rather than replaying the cached blank.
    #[test]
    fn visible_image_records_pending_and_fingerprint_flips_on_transmit() {
        let chat = chat_with_image_entry();
        let store = Rc::new(RefCell::new(ImageStore::default()));
        let builder = image_builder(&chat, &store);
        let ctx = crate::test_support::draw_ctx(60, None);

        let s1 = builder
            .item_at_idx(0, 0)
            .expect("entry")
            .borrow_mut()
            .draw(&ctx);
        assert!(
            surface_rows(&s1)
                .iter()
                .flatten()
                .all(|c| c.image.is_none()),
            "no placement before the id lands",
        );
        let pending = store.borrow_mut().take_pending();
        assert_eq!(pending.len(), 1, "the visible image was recorded pending");

        let (agent, entry_id) = pending[0];
        store.borrow_mut().insert(agent, entry_id, 7);
        let s2 = builder
            .item_at_idx(0, 0)
            .expect("entry")
            .borrow_mut()
            .draw(&ctx);
        let placement = surface_rows(&s2)
            .into_iter()
            .flatten()
            .find_map(|c| c.image);
        assert!(
            matches!(placement, Some(p) if p.img_id == 7),
            "the fingerprint flip rebuilds and places the image: {placement:?}",
        );
    }

    /// Caps on but `show_image_in_terminal` off: the tool image cell emits the
    /// `[image: ...]` text fallback, records no pending key, and writes no
    /// placement. The config half of the gate is load-bearing (dropping it
    /// from `resolve_image` records a pending key and draws the blank reserve
    /// instead).
    #[test]
    fn config_off_falls_back_to_text_and_records_no_pending() {
        let chat = chat_with_image_entry();
        chat.borrow_mut().show_image_in_terminal = false;
        let store = Rc::new(RefCell::new(ImageStore::default()));
        let builder = image_builder(&chat, &store);
        let ctx = crate::test_support::draw_ctx(60, None);

        let surface = builder
            .item_at_idx(0, 0)
            .expect("entry")
            .borrow_mut()
            .draw(&ctx);

        let rows = crate::test_support::rows(&surface);
        assert!(
            rows.iter().any(|r| r.contains("[image:")),
            "text fallback shown while the config is off: {rows:?}",
        );
        assert!(
            surface_rows(&surface)
                .iter()
                .flatten()
                .all(|c| c.image.is_none()),
            "no placement while the config is off",
        );
        assert!(
            store.borrow_mut().take_pending().is_empty(),
            "no pending key recorded while the config is off",
        );
    }

    /// A tool image whose transmit gave up (`Failed`) renders the
    /// `[image: ...]` text fallback, writes no placement, and records no
    /// pending key, so the host never re-attempts it. Treating `Failed` as
    /// `Pending` in the tool cell would draw the blank reserve and redden the
    /// text assertion; recording it pending would redden the last assertion.
    #[test]
    fn failed_image_falls_back_to_text_and_records_no_pending() {
        let chat = chat_with_image_entry();
        let entry_id = chat
            .borrow()
            .transcript(AgentId::Main)
            .expect("main transcript")
            .entries()
            .iter()
            .find(|e| matches!(&e.kind, EntryKind::Tool(_)))
            .expect("tool image entry")
            .id;
        let store = Rc::new(RefCell::new(ImageStore::default()));
        store.borrow_mut().mark_failed(AgentId::Main, entry_id);
        let builder = image_builder(&chat, &store);
        let ctx = crate::test_support::draw_ctx(60, None);

        let surface = builder
            .item_at_idx(0, 0)
            .expect("entry")
            .borrow_mut()
            .draw(&ctx);

        let rows = crate::test_support::rows(&surface);
        assert!(
            rows.iter().any(|r| r.contains("[image:")),
            "text fallback shown for a failed image: {rows:?}",
        );
        assert!(
            surface_rows(&surface)
                .iter()
                .flatten()
                .all(|c| c.image.is_none()),
            "no placement for a failed image",
        );
        assert!(
            store.borrow_mut().take_pending().is_empty(),
            "a failed image records no pending key",
        );
    }

    /// Toggling `show_image_in_terminal` on while the image is still pending
    /// clears the render cache wholesale, so the stale text fallback is not
    /// replayed. The per-entry fingerprint cannot catch this (the id is absent
    /// in both the disabled and pending states), so `GlobalRenderInputs`
    /// carries the toggle. Dropping `show_image_in_terminal` from
    /// `GlobalRenderInputs` replays the cached text and reddens this test.
    #[test]
    fn config_toggle_clears_cache_so_no_stale_text() {
        let chat = chat_with_image_entry();
        // Start with the config off so the first frame caches the text
        // fallback under this entry's key.
        chat.borrow_mut().show_image_in_terminal = false;
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);
        let mut view = TranscriptView::new(
            Rc::clone(&chat),
            &theme,
            Rc::new(std::cell::Cell::new(false)),
            Rc::new(RefCell::new(None)),
            Rc::new(std::cell::Cell::new(None)),
            Rc::new(RefCell::new(ImageStore::default())),
        );
        // Caps on, so only the config gate decides.
        view.set_styles(Rc::new(TranscriptStyles::from_theme(
            &theme,
            TerminalCaps {
                images: true,
                ..TerminalCaps::default()
            },
        )));
        let ctx = draw_ctx(48, 40);

        let rows0 = crate::test_support::rows(&view.draw(&ctx));
        assert!(
            rows0.iter().any(|r| r.contains("[image:")),
            "text fallback while the config is off: {rows0:?}",
        );

        // Toggle on: the image is still untransmitted (Pending), so it draws
        // the blank reserve, not the stale text. Only the wholesale clear can
        // rebuild it, since the per-entry fingerprint did not change.
        chat.borrow_mut().show_image_in_terminal = true;
        let rows1 = crate::test_support::rows(&view.draw(&ctx));
        assert!(
            !rows1.iter().any(|r| r.contains("[image:")),
            "no stale text after toggling images on: {rows1:?}",
        );
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
        let mut widget = build_entry_widget(
            entry,
            &chat,
            &builder.styles,
            false,
            None,
            ImageRender::Disabled,
        )
        .into_indented_boxed();
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

    /// A notice row, the cheapest entry whose fingerprint is a length proxy.
    fn notice(text: &str) -> AgentEvent {
        AgentEvent::Notice {
            agent_id: AgentId::Main,
            text: text.to_string(),
        }
    }

    fn user_end(text: &str) -> AgentEvent {
        AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::User(aj_models::types::UserMessage::text(text))),
        }
    }

    fn task_notification_end(label: &str, outcome: TaskOutcome, body: &str) -> AgentEvent {
        AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: AgentMessage::task_notification(aj_agent::message::TaskNotification::new(
                label.into(),
                aj_agent::message::TaskNotificationKind::Bash,
                outcome,
                body.into(),
            )),
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

    #[test]
    fn same_length_diff_content_changes_the_fingerprint() {
        fn fingerprint(details: &ToolDetails) -> u64 {
            let mut hasher = DefaultHasher::new();
            details_fingerprint(details, &mut hasher);
            hasher.finish()
        }

        let first = ToolDetails::Diff(DiffDetails::new("x.txt", "old\n", "new\n"));
        let second = ToolDetails::Diff(DiffDetails::new("x.txt", "bad\n", "yay\n"));

        assert_ne!(fingerprint(&first), fingerprint(&second));
    }

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
        let update = |stdout: &str| AgentEvent::ToolExecutionUpdate {
            agent_id: AgentId::Main,
            call_id: "c1".into(),
            tool: "bash".into(),
            args: serde_json::json!({}),
            partial: bash("run", stdout, None, None),
            content: Vec::new().into(),
        };
        apply(&chat, &mut life, update("line 1\n"));
        let builder = caching_builder(&chat);
        let first = draw_and_assert_fresh(&builder, AgentId::Main, 0, 60);

        // The snapshot grows while the call is still running.
        apply(&chat, &mut life, update("line 1\nline 2\nline 3\n"));
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
            tool_end(
                AgentId::Main,
                "c1",
                "bash",
                bash("sleep 1", "", None, Some(1)),
            ),
        );
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

    /// The fingerprint distinguishes user-entry content, so a slot could
    /// never serve one user render for another. User entries are immutable
    /// after append, so this fingerprint sensitivity is the anti-stale
    /// guarantee for them.
    #[test]
    fn user_entry_fingerprint_tracks_content() {
        let hello = transcript_with(EntryKind::User(UserEntry {
            content: vec![UserContent::text("hello")],
            message_id: None,
        }));
        let longer = transcript_with(EntryKind::User(UserEntry {
            content: vec![UserContent::text("hello, world")],
            message_id: None,
        }));
        let chat = empty_chat();
        let fp = |t: &Transcript| entry_fingerprint(&t.entries()[0], &chat.borrow());
        assert_ne!(fp(&hello), fp(&longer), "content length is fingerprinted");
    }

    /// A notification's outcome is fingerprinted, since it drives the bubble
    /// tint: two notices identical but for their outcome must not share a
    /// cached surface.
    #[test]
    fn task_notification_fingerprint_tracks_outcome() {
        let make = |outcome: TaskOutcome| {
            transcript_with(EntryKind::TaskNotification(TaskNotificationEntry {
                message_id: None,
                label: "sleep".into(),
                kind: aj_agent::message::TaskNotificationKind::Bash,
                outcome,
                body: "exit".into(),
            }))
        };
        let ok = make(TaskOutcome::Succeeded);
        let bad = make(TaskOutcome::Failed { code: Some(1) });
        let chat = empty_chat();
        let fp = |t: &Transcript| entry_fingerprint(&t.entries()[0], &chat.borrow());
        assert_ne!(fp(&ok), fp(&bad), "outcome is fingerprinted");
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
            entry: None,
        }));
        let other_summary = transcript_with(EntryKind::Compaction(CompactionEntry {
            tokens_before: 100_000,
            tokens_after: 25_000,
            summary: "one two".into(),
            entry: None,
        }));
        let other_tokens = transcript_with(EntryKind::Compaction(CompactionEntry {
            tokens_before: 90_000,
            tokens_after: 25_000,
            summary: "one".into(),
            entry: None,
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
        spawn_sub_id(chat, life, 0);
    }

    fn spawn_sub_id(chat: &Rc<RefCell<ChatState>>, life: &mut AgentLifecycle, child: usize) {
        apply(
            chat,
            life,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(child),
                task: if child == 0 {
                    "scout the code".into()
                } else {
                    format!("scout the code as agent {child}")
                },
                background: false,
                settings: cache_settings(),
            },
        );
        apply(
            chat,
            life,
            AgentEvent::MessageEnd {
                agent_id: AgentId::Sub(child),
                message: AgentMessage::wire(Message::Assistant(text_message("starting"))),
            },
        );
    }

    /// A `Running` box bypasses the render cache: it rebuilds on every draw
    /// (its glyph animates on the wall-clock), so it never records a hit and
    /// always reflects the latest live activity. The box keys on its own
    /// metadata, not a child fold.
    #[test]
    fn running_box_bypasses_cache_and_reflects_activity() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        spawn_sub(&chat, &mut life);
        let builder = caching_builder(&chat);
        // Two unchanged draws: a cached entry would hit the second time, but a
        // Running box bypasses the cache and never populates it. We draw
        // through the cache path directly rather than compare against a fresh
        // render, because a bypass box has no cached surface to go stale (and
        // its wall-clock glyph would differ between two builds anyway).
        let first = draw_cached(&builder, 0, 70);
        let _ = draw_cached(&builder, 0, 70);
        assert_eq!(hits(&builder), 0, "a running box never hits the cache");
        assert_eq!(misses(&builder), 0, "and never populates it");

        apply(&chat, &mut life, tool_start(AgentId::Sub(0), "c1", "grep"));
        let after = draw_cached(&builder, 0, 70);
        assert_ne!(
            crate::test_support::flatten(&first),
            crate::test_support::flatten(&after),
            "the running body changed",
        );
        assert!(
            crate::test_support::rows(&after)
                .join("\n")
                .contains("grep"),
            "the box shows the new latest activity: {:?}",
            crate::test_support::rows(&after),
        );
    }

    /// A live conclusion update (a fresh assistant `MessageEnd` on the still
    /// running sub) refreshes the box's latest-activity line, and because the
    /// box bypasses the cache the next draw reflects it.
    #[test]
    fn running_box_reflects_a_conclusion_update() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        spawn_sub(&chat, &mut life);
        let builder = caching_builder(&chat);
        let first = draw_cached(&builder, 0, 70);

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
        let grown = draw_cached(&builder, 0, 70);
        assert_ne!(
            crate::test_support::flatten(&first),
            crate::test_support::flatten(&grown),
            "the box render actually changed",
        );
    }

    /// A concluded (`Done`) box is cacheable, unlike a `Running` one: it reads
    /// no wall-clock, so a redraw with no metadata change hits the cache. This
    /// guards the `status == Running` guard on the bypass predicate: widening
    /// it would rebuild every concluded box each frame.
    #[test]
    fn done_box_is_cached() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        spawn_sub(&chat, &mut life);
        apply(
            &chat,
            &mut life,
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(0),
                report: "all done".into(),
                conclusion: aj_agent::events::SubAgentConclusion::Completed,
            },
        );
        apply(
            &chat,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(0),
                messages: Vec::new(),
            },
        );
        let builder = caching_builder(&chat);
        let _ = draw_cached(&builder, 0, 60);
        let _ = draw_cached(&builder, 0, 60);
        assert!(hits(&builder) > 0, "a Done box hits the cache on redraw");
    }

    /// A `Running` box reads no child transcript, so child-only changes (a
    /// notice appended to the child, a background task's terminal badge flip)
    /// never appear in the box. This is the metadata decoupling that lets a
    /// resumed sub-agent's transcript stay unmaterialized behind the box. The
    /// running box also bypasses the render cache, so it records no hits.
    #[test]
    fn running_box_ignores_child_only_changes() {
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
        // A bash tool cell backed by a background task, so a later `TaskEnd`
        // is a pure child-transcript change (a badge flip).
        apply(&chat, &mut life, tool_start(AgentId::Sub(0), "c1", "bash"));
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
        let builder = caching_builder(&chat);
        draw_cached(&builder, 0, 70);

        // A notice appended to the child transcript touches no box metadata.
        apply(
            &chat,
            &mut life,
            AgentEvent::Notice {
                agent_id: AgentId::Sub(0),
                text: "sub-child-marker".into(),
            },
        );
        let _ = draw_cached(&builder, 0, 70);
        // A background task's terminal badge flip is likewise child-only.
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
        let after = draw_cached(&builder, 0, 70);

        // A running box bypasses the cache, so it never serves a cached
        // surface: three draws record no hits. If it were cached, the
        // child-only changes (which touch no box metadata) would hit.
        assert_eq!(
            hits(&builder),
            0,
            "a running box bypasses the render cache entirely",
        );
        let body = crate::test_support::rows(&after).join("\n");
        assert!(
            !body.contains("sub-child-marker"),
            "the child notice is not shown in the box: {body}",
        );
        assert!(
            !body.contains("[task #1]"),
            "the child tool cell is not shown in the box: {body}",
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

    /// Toggling `show_thinking_block` clears the whole cache.
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

        chat.borrow_mut().show_thinking_block = false;
        let _ = view.draw(&ctx);
        assert!(
            view.cache.borrow().misses > misses_before,
            "toggling show_thinking_block forced misses",
        );
        let rows = crate::test_support::rows(&view.draw(&ctx));
        assert!(
            rows.join("\n").contains("Thinking…"),
            "placeholder shown: {rows:?}"
        );
    }

    /// Token-usage rows disappear when `show_token_usage` is off, and the
    /// toggle clears the cache so the change repaints.
    #[test]
    fn toggling_show_token_usage_hides_the_rows_and_clears_the_cache() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(
            &chat,
            &mut life,
            AgentEvent::UsageUpdate {
                agent_id: AgentId::Main,
                usage: aj_agent::types::TokenUsage {
                    accumulated_input: 100,
                    turn_input: 100,
                    accumulated_output: 50,
                    turn_output: 50,
                    accumulated_cache_write: 0,
                    turn_cache_write: 0,
                    accumulated_cache_read: 0,
                    turn_cache_read: 0,
                },
            },
        );
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(60, 24);
        // Default on: the usage row renders.
        assert!(
            crate::test_support::rows(&view.draw(&ctx))
                .join("\n")
                .contains("Token Usage"),
            "usage row shown by default",
        );
        let _ = view.draw(&ctx);
        let misses_before = view.cache.borrow().misses;

        chat.borrow_mut().show_token_usage = false;
        let rows = crate::test_support::rows(&view.draw(&ctx));
        assert!(
            view.cache.borrow().misses > misses_before,
            "toggling show_token_usage forced misses",
        );
        assert!(
            !rows.join("\n").contains("Token Usage"),
            "usage row hidden: {rows:?}"
        );
    }

    /// A hidden token-usage row occupies zero rows, so the entry below it must
    /// still map to its own screen row. Regression: the per-entry row-walk once
    /// floored every entry to one row, which drifted selection and mouse
    /// hit-testing by one row for everything below a hidden row.
    #[test]
    fn hidden_usage_row_keeps_the_row_walk_aligned() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(
            &chat,
            &mut life,
            AgentEvent::Notice {
                agent_id: AgentId::Main,
                text: "AAA".into(),
            },
        );
        apply(
            &chat,
            &mut life,
            AgentEvent::UsageUpdate {
                agent_id: AgentId::Main,
                usage: aj_agent::types::TokenUsage {
                    accumulated_input: 1,
                    turn_input: 1,
                    accumulated_output: 1,
                    turn_output: 1,
                    accumulated_cache_write: 0,
                    turn_cache_write: 0,
                    accumulated_cache_read: 0,
                    turn_cache_read: 0,
                },
            },
        );
        apply(
            &chat,
            &mut life,
            AgentEvent::Notice {
                agent_id: AgentId::Main,
                text: "BBB".into(),
            },
        );
        chat.borrow_mut().show_token_usage = false;

        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let rows = crate::test_support::rows(&view.draw(&ctx));
        let bbb_row = rows
            .iter()
            .position(|r| r.contains("BBB"))
            .expect("BBB visible");

        // A click on BBB's screen row resolves to the Notice B entry, not the
        // hidden usage row sitting (invisibly) between A and B.
        let usage = entry_id(&chat, 1);
        let notice_b = entry_id(&chat, 2);
        let pos = view
            .point_to_sel(i16::try_from(bbb_row).unwrap(), 1)
            .expect("resolves to an entry");
        assert_eq!(pos.entry, notice_b, "click on BBB maps to Notice B");
        assert_ne!(pos.entry, usage, "never the hidden usage row");
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

    /// A reset is a new incarnation of the model, and its `EntryId(0)` is not
    /// the old one's, so no cache slot survives it.
    ///
    /// The collision this rules out is cheap to hit rather than exotic: entry
    /// ids restart at 0, the surface cache is keyed `(AgentId, EntryId)`, and
    /// the fingerprint that validates a slot is a length proxy, so two notices
    /// of the same length under the same id agree on every part of the key.
    /// Nothing about the *entry* has changed in a way a fingerprint can see,
    /// because it is a different entry.
    #[test]
    fn a_reset_retires_the_slots_of_the_incarnation_it_ended() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(&chat, &mut life, notice("alpha"));
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(60, 24);
        let _ = view.draw(&ctx);
        let first = crate::test_support::rows(&view.draw(&ctx)).join("\n");
        assert!(
            first.contains("alpha"),
            "the first incarnation drew: {first}"
        );
        let retired_id = entry_id(&chat, 0);
        assert!(
            view.cache.borrow().hits > 0,
            "nothing was cached, so there is no stale slot for the reset to \
             retire and this test measures nothing",
        );

        // A different notice of the same length at the same id: same kind, same
        // length, same width, same agent, so the whole key and its validation
        // agree with the retired slot's.
        chat.borrow_mut().reset(&mut life);
        apply(&chat, &mut life, notice("omega"));
        assert_eq!(
            entry_id(&chat, 0),
            retired_id,
            "the new entry must reuse the retired one's id, or nothing collides",
        );

        let rows = crate::test_support::rows(&view.draw(&ctx)).join("\n");
        assert!(
            rows.contains("omega") && !rows.contains("alpha"),
            "the new incarnation replayed the surface of the one it replaced: {rows}",
        );
    }

    /// The same retirement, on the path that never draws.
    ///
    /// Select-to-copy lays entries out through the text cache on demand, so a
    /// clear hung off the draw would not cover it: this asks for the text of
    /// `EntryId(0)` before and after a reset and must not be told the old
    /// incarnation's.
    #[test]
    fn a_reset_retires_the_text_of_the_incarnation_it_ended() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(&chat, &mut life, notice("alpha"));
        let mut view = transcript_view(&chat);

        let text_of = |view: &mut TranscriptView, chat: &Rc<RefCell<ChatState>>| {
            let id = entry_id(chat, 0);
            view.entry_rows(id, 60)
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|cell| cell.char.grapheme())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("")
        };
        let before = text_of(&mut view, &chat);
        assert!(
            before.contains("alpha"),
            "the fixture never laid the first entry out, so there is nothing \
             cached to go stale: {before:?}",
        );

        chat.borrow_mut().reset(&mut life);
        apply(&chat, &mut life, notice("omega"));
        let after = text_of(&mut view, &chat);
        assert!(
            after.contains("omega") && !after.contains("alpha"),
            "select-to-copy read the previous incarnation's rows: {after:?}",
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
        // A cacheable Main entry: the running sub box bypasses the cache, so
        // without another entry the Main view would record no hits to clear.
        apply(&chat, &mut life, user_end("hi"));
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
    /// are unchanged, so the draw-time global clear does not fire either.
    ///
    /// The path as the shell runs it, `reset_to_tail` and all. That call clears
    /// both caches whatever the incarnation says, so this passes under a
    /// retirement that cannot tell a swap from a reset. What the retirement
    /// itself is worth is the test below.
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
            let _ = reduce(&mut fresh, &mut fresh_life, user_end("world"), None);
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

    /// The same swap with nothing papering it: the retirement alone keeps the
    /// fresh session off the previous one's surface, with no clear from the
    /// call site.
    ///
    /// This is the one test that reaches the claim the change rests on, that no
    /// call site has to remember. Every other test of the retirement is passed
    /// by a counter that cannot tell a swap from a reset: the two reset tests go
    /// through `ChatState::reset`, which bumps a per-model counter just as well
    /// as a process-global one, and the twin above papers with `reset_to_tail`.
    /// Here a per-model counter, minted at 0 in `new` and bumped only by
    /// `reset`, hands the fresh model the generation the outgoing one already
    /// had, the collided slot is not retired, and this reads the stale "hello".
    #[test]
    fn a_swapped_in_session_needs_no_clear_from_its_call_site() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        apply(&chat, &mut life, user_end("hello"));
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(60, 24);
        let _ = view.draw(&ctx);
        let first = crate::test_support::rows(&view.draw(&ctx));
        assert!(
            first.join("\n").contains("hello"),
            "the outgoing session was never drawn, so there is no surface here \
             for a swap to replay: {first:?}",
        );
        assert!(
            view.cache.borrow().hits > 0,
            "the first session's slot was never cached, so there is nothing \
             here for a swap to collide with",
        );

        // The same collision the twin above builds: `EntryId(0)` again, content
        // of the same length so the fingerprint matches, and globals that match
        // so the draw-time clear stays quiet.
        {
            let mut fresh = ChatState::new(cache_settings(), 0, Arc::new(Vec::new()));
            let mut fresh_life = AgentLifecycle::default();
            let _ = reduce(&mut fresh, &mut fresh_life, user_end("world"), None);
            *chat.borrow_mut() = fresh;
        }
        // And no `reset_to_tail`. Nothing between the swap and the draw tells
        // the view anything happened.

        let rows = crate::test_support::rows(&view.draw(&ctx)).join("\n");
        assert!(
            rows.contains("world") && !rows.contains("hello"),
            "the swapped-in session was served the previous one's cached \
             surface, so the retirement needs a call site to remember after \
             all: {rows:?}",
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
        // size / width method the per-entry provider reuses. The transcript
        // bottom-anchors, so content shorter than the viewport ends at the
        // bottom edge rather than starting at row 0.
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
        let total = rows0.len() + rows1.len();
        assert!(
            total < usize::from(vh),
            "content fits, so the list bottom-anchors it below row 0",
        );

        // Bottom-anchored: the content ends at the viewport bottom, so it
        // starts at `vh - total`. Entry 0 occupies the first `rows0.len()`
        // content rows, entry 1 the ones right after it.
        let start = usize::from(vh) - total;
        for (r, line) in rows0.iter().enumerate() {
            assert_eq!(
                &grid[start + r][..usize::from(content_w)],
                line.as_slice(),
                "entry 0 row {r} differs from the visible render",
            );
        }
        let base = start + rows0.len();
        for (r, line) in rows1.iter().enumerate() {
            assert_eq!(
                &grid[base + r][..usize::from(content_w)],
                line.as_slice(),
                "entry 1 row {r} differs from the visible render",
            );
        }
        // The reserved scrollbar column stays blank while the transcript fits.
        for r in start..usize::from(vh) {
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
        assert_eq!(view.extract_selection(w, anchor, caret), "row 0\n\nrow 1");
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
    /// rows and columns, skips the leading chrome margin, trims trailing pad
    /// per line, and joins rows with `\n`. This is the panic-safety net when a
    /// stale selection outlives a width or content change, so it is exercised
    /// directly.
    #[test]
    fn extract_from_lines_reads_normalized_ranges() {
        let lines = vec![cells(" row 0"), cells(""), cells(" row 1")];
        // Forward substring within a line, and the same range reversed.
        assert_eq!(extract_from_lines(&lines, (0, 1), (0, 4)), "row");
        assert_eq!(extract_from_lines(&lines, (0, 4), (0, 1)), "row");
        // A range that starts in the margin skips it and trims the trailing pad.
        assert_eq!(extract_from_lines(&lines, (0, 0), (0, 40)), "row 0");
        // Multi-line join keeps the blank middle row.
        assert_eq!(extract_from_lines(&lines, (0, 1), (2, 6)), "row 0\n\nrow 1");
        // An out-of-range end row clamps to the last line.
        assert_eq!(
            extract_from_lines(&lines, (0, 1), (99, 3)),
            "row 0\n\nrow 1"
        );
        // A degenerate range is empty.
        assert_eq!(extract_from_lines(&lines, (1, 0), (1, 0)), "");

        let spaced = cells("a   b");
        assert_eq!(
            cell_range_text(&spaced, 1, 4),
            "   ",
            "selected whitespace is not mistaken for layout padding",
        );

        let wide = vec![
            Cell {
                char: Character::new("中", 2),
                ..Cell::default()
            },
            Cell::default(),
            Cell {
                char: Character::new("x", 1),
                ..Cell::default()
            },
        ];
        assert_eq!(cell_range_text(&wide, 0, 3), "中x");
        assert_eq!(
            cell_range_text(&wide, 1, 2),
            "中",
            "a continuation-column range expands to the complete grapheme",
        );
    }

    #[test]
    fn word_classes_match_terminal_selection_conventions() {
        assert_eq!(word_class(" "), WordClass::Whitespace);
        assert_eq!(word_class("\t"), WordClass::Whitespace);
        assert_eq!(word_class("/"), WordClass::Delimiter);
        assert_eq!(word_class("-"), WordClass::Delimiter);
        assert_eq!(word_class("_"), WordClass::Regular);
        assert_eq!(word_class("é"), WordClass::Regular);
        assert_eq!(word_class("🦀"), WordClass::Regular);
    }

    #[test]
    fn click_count_requires_nearby_presses_inside_the_interval() {
        let chat = chat_with_notices(1);
        let mut view = transcript_view(&chat);
        let pos = SelPos {
            entry: entry_id(&chat, 0),
            line: 0,
            col: 2,
        };
        let now = Instant::now();
        assert_eq!(view.click_count(pos, now), 1);
        assert_eq!(view.click_count(pos, now + Duration::from_millis(499)), 2);
        assert_eq!(
            view.click_count(pos, now + Duration::from_millis(1_000)),
            1,
            "the interval is measured from the immediately preceding press",
        );
        assert_eq!(
            view.click_count(SelPos { col: 4, ..pos }, now + Duration::from_millis(1_001),),
            1,
            "movement beyond one cell starts a new sequence",
        );
    }

    /// `content_start` skips at most the one-column chrome margin: a plain
    /// row's text column is found, while content-level leading whitespace past
    /// the margin (a code block's indentation) is preserved, and a row with no
    /// margin keeps its first column.
    #[test]
    fn content_start_skips_only_the_chrome_margin() {
        assert_eq!(content_start(&cells("code")), 0, "no margin");
        assert_eq!(content_start(&cells(" text")), 1, "one blank margin column");
        assert_eq!(
            content_start(&cells("   deep")),
            1,
            "margin skipped, content indentation kept",
        );
        assert_eq!(
            content_start(&cells("    ")),
            1,
            "fully blank row caps at margin"
        );
        assert_eq!(content_start(&cells("")), 0, "empty row has no margin");
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
        // Interior row (entry 0's blank spacer): highlighted from the content
        // column to the edge, skipping the chrome margin (semantic selection).
        assert_ne!(grid[1][0].style.bg, bg, "chrome margin is not highlighted");
        assert_eq!(grid[1][1].style.bg, bg, "interior row painted from col 1");
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

        // The covered cells carry the selection background, except the chrome
        // margin (col 0), which semantic selection never highlights.
        assert_ne!(
            grid[2][0].style.bg, bg,
            "chrome margin (2,0) is not highlighted"
        );
        for c in 1..6 {
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

    #[test]
    fn plain_click_on_subagent_box_observes_agent_but_edges_and_drags_do_not() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        spawn_sub(&chat, &mut life);
        let mut view = transcript_view(&chat);
        let observed = Rc::new(std::cell::Cell::new(None));
        let observed_c = Rc::clone(&observed);
        view.set_on_observe_agent(Box::new(move |id| observed_c.set(Some(id))));
        let ctx = draw_ctx(40, 20);
        let _ = view.draw(&ctx);

        let id = entry_id(&chat, 0);
        let positions = view.visible_row_positions(20);
        let box_height = view.entry_height(id, view.content_width());
        let box_row = positions
            .iter()
            .position(|pos| matches!(pos, Some(pos) if pos.entry == id && pos.line == 0))
            .expect("first box row is visible");
        let spacer_row = positions
            .iter()
            .position(
                |pos| matches!(pos, Some(pos) if pos.entry == id && pos.line + 1 == box_height),
            )
            .expect("box spacer is visible");
        let blank_row = positions
            .iter()
            .position(Option::is_none)
            .expect("short transcript leaves a blank top band");
        let box_row = i16::try_from(box_row).expect("row fits");
        let spacer_row = i16::try_from(spacer_row).expect("row fits");
        let blank_row = i16::try_from(blank_row).expect("row fits");

        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(5, box_row, mouse::Type::Press));
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(5, box_row, mouse::Type::Release));
        assert_eq!(observed.take(), Some(AgentId::Sub(0)));

        for row in [spacer_row, blank_row] {
            let mut ec = EventContext::new();
            view.handle_event(&mut ec, &mouse(5, row, mouse::Type::Press));
            let mut ec = EventContext::new();
            view.handle_event(&mut ec, &mouse(5, row, mouse::Type::Release));
            assert_eq!(observed.get(), None, "row {row} is not clickable");
        }

        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(5, box_row, mouse::Type::Press));
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(6, box_row, mouse::Type::Drag));
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(6, box_row, mouse::Type::Release));
        assert_eq!(observed.get(), None, "a selection drag does not navigate");

        let mut ec = EventContext::new();
        view.handle_event(
            &mut ec,
            &mouse_with_mods(5, box_row, mouse::Type::Press, mouse::Modifiers::SHIFT),
        );
        let mut ec = EventContext::new();
        view.handle_event(
            &mut ec,
            &mouse_with_mods(5, box_row, mouse::Type::Release, mouse::Modifiers::SHIFT),
        );
        assert_eq!(observed.get(), None, "a modified click does not navigate");
    }

    #[test]
    fn agent_hit_testing_uses_the_last_drawn_geometry() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        spawn_sub_id(&chat, &mut life, 0);
        spawn_sub_id(&chat, &mut life, 1);
        let mut view = transcript_view(&chat);
        let observed = Rc::new(std::cell::Cell::new(None));
        let observed_c = Rc::clone(&observed);
        view.set_on_observe_agent(Box::new(move |id| observed_c.set(Some(id))));
        view.follow_tail = false;
        view.list.borrow_mut().scroll_lines(-1000);
        let ctx = draw_ctx(40, 10);
        let _ = view.draw(&ctx);
        let old_agent_1_row = view
            .agent_hit_rows
            .iter()
            .position(|agent| *agent == Some(AgentId::Sub(1)))
            .expect("agent 1 is visible in the rendered frame");

        let report = (0..20)
            .map(|line| format!("report line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        apply(
            &chat,
            &mut life,
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(0),
                report,
                conclusion: aj_agent::events::SubAgentConclusion::Completed,
            },
        );
        apply(
            &chat,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(0),
                messages: Vec::new(),
            },
        );

        let row = i16::try_from(old_agent_1_row).expect("row fits");
        assert_eq!(
            view.subagent_at_point(row, 5),
            Some(AgentId::Sub(1)),
            "live geometry changes do not alter the displayed frame's hit map",
        );
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(5, row, mouse::Type::Press));
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(5, row, mouse::Type::Release));
        assert_eq!(observed.get(), Some(AgentId::Sub(1)));

        let _ = view.draw(&ctx);
        assert_eq!(
            view.subagent_at_point(row, 5),
            Some(AgentId::Sub(0)),
            "a redraw installs the expanded box's geometry",
        );
    }

    #[test]
    fn mouse_leave_reset_and_capture_drag_cancel_agent_clicks() {
        let chat = empty_chat();
        let mut life = AgentLifecycle::default();
        spawn_sub(&chat, &mut life);
        let mut view = transcript_view(&chat);
        let observed = Rc::new(std::cell::Cell::new(None));
        let observed_c = Rc::clone(&observed);
        view.set_on_observe_agent(Box::new(move |id| observed_c.set(Some(id))));
        let ctx = draw_ctx(40, 20);
        let _ = view.draw(&ctx);
        let row = i16::try_from(
            view.agent_hit_rows
                .iter()
                .position(Option::is_some)
                .expect("box row is visible"),
        )
        .expect("row fits");

        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(5, row, mouse::Type::Press));
        view.handle_event(&mut ec, &Event::MouseLeave);
        view.handle_event(&mut ec, &mouse(5, row, mouse::Type::Release));
        assert_eq!(observed.get(), None, "MouseLeave cancels the click");

        view.handle_event(&mut ec, &mouse(5, row, mouse::Type::Press));
        view.reset_to_tail();
        assert_eq!(
            view.subagent_at_point(row, 5),
            None,
            "reset drops stale rendered geometry",
        );
        view.handle_event(&mut ec, &mouse(5, row, mouse::Type::Release));
        assert_eq!(observed.get(), None, "reset cancels the click");

        let _ = view.draw(&ctx);
        view.handle_event(&mut ec, &mouse(5, row, mouse::Type::Press));
        view.capture_event(&mut ec, &mouse(6, row, mouse::Type::Drag));
        assert_eq!(
            view.agent_click, None,
            "capture-phase drag cancels the click"
        );
        view.handle_event(&mut ec, &mouse(6, row, mouse::Type::Release));
        assert_eq!(observed.get(), None, "drag does not navigate");

        view.handle_event(&mut ec, &mouse(5, row, mouse::Type::Press));
        let mut wheel = match mouse(5, row, mouse::Type::Press) {
            Event::Mouse(mouse) => mouse,
            _ => unreachable!(),
        };
        wheel.button = mouse::Button::WheelUp;
        view.handle_event(&mut ec, &Event::Mouse(wheel));
        view.handle_event(&mut ec, &mouse(5, row, mouse::Type::Release));
        assert_eq!(observed.get(), None, "wheel input cancels the click");
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
        assert_eq!(copied.as_deref(), Some("row 1"), "copied the selection");
        assert!(
            view.selection.is_some(),
            "a real range stays highlighted after copy",
        );
    }

    #[test]
    fn double_click_selects_and_drags_by_terminal_word_classes() {
        let chat = chat_with_notices(20);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        view.follow_tail = false;
        view.list.borrow_mut().scroll_lines(-1000);
        let _ = view.draw(&ctx);

        // The first click starts the multi-click sequence. The second press on
        // the `o` in "row 1" selects the complete regular-character run.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(2, 2, mouse::Type::Press));
        view.handle_event(&mut ec, &mouse(2, 2, mouse::Type::Release));
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(2, 2, mouse::Type::Press));
        let word = view.selection.expect("double-click selected a word");
        assert_eq!(
            view.extract_selection(view.content_width(), word.anchor, word.caret),
            "row",
        );

        // Dragging onto the digit snaps the moving edge to that whole run and
        // retains the whitespace between the two words.
        view.handle_event(&mut ec, &mouse(5, 2, mouse::Type::Drag));
        let selection = view.selection.expect("word drag kept a selection");
        assert_eq!(
            view.extract_selection(view.content_width(), selection.anchor, selection.caret,),
            "row 1",
        );
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(5, 2, mouse::Type::Release));
        let copied = ec.cmds.iter().find_map(|cmd| match cmd {
            vaxis::vxfw::Command::CopyToClipboard(text) => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(copied, Some("row 1"));
    }

    #[test]
    fn triple_click_selects_the_complete_rendered_line() {
        let chat = chat_with_notices(20);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        view.follow_tail = false;
        view.list.borrow_mut().scroll_lines(-1000);
        let _ = view.draw(&ctx);

        for _ in 0..2 {
            let mut ec = EventContext::new();
            view.handle_event(&mut ec, &mouse(2, 2, mouse::Type::Press));
            view.handle_event(&mut ec, &mouse(2, 2, mouse::Type::Release));
        }
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(2, 2, mouse::Type::Press));
        let line = view.selection.expect("triple-click selected a line");
        assert_eq!(
            view.extract_selection(view.content_width(), line.anchor, line.caret),
            "row 1",
        );
        assert_eq!(line.anchor.col, 0, "line selection starts at the edge");
        assert_eq!(
            line.caret.col,
            usize::from(view.content_width()),
            "line selection reaches the right edge",
        );

        view.handle_event(&mut ec, &mouse(2, 2, mouse::Type::Release));
        let copied = ec.cmds.iter().find_map(|cmd| match cmd {
            vaxis::vxfw::Command::CopyToClipboard(text) => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(copied, Some("row 1"));
    }

    /// A select-to-copy release records the copied character count in the
    /// shared cell the toast reads, while a plain click records nothing.
    #[test]
    fn release_records_the_copied_character_count() {
        let chat = chat_with_notices(20);
        let theme = Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor);
        let selection_copied = Rc::new(std::cell::Cell::new(None));
        let mut view = TranscriptView::new(
            Rc::clone(&chat),
            &theme,
            Rc::new(std::cell::Cell::new(false)),
            Rc::new(RefCell::new(None)),
            Rc::clone(&selection_copied),
            Rc::new(RefCell::new(ImageStore::default())),
        );
        let ctx = draw_ctx(40, 10);
        view.follow_tail = false;
        view.list.borrow_mut().scroll_lines(-1000);
        let _ = view.draw(&ctx);

        // A plain click (no drag) records nothing.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(2, 2, mouse::Type::Press));
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(2, 2, mouse::Type::Release));
        assert!(
            selection_copied.get().is_none(),
            "a plain click records no copy"
        );

        // Select " row 1" (entry 1, screen row 2): press at col 0, drag to
        // col 6, release. The chrome margin is skipped, so "row 1" is copied.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(0, 2, mouse::Type::Press));
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(6, 2, mouse::Type::Drag));
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(6, 2, mouse::Type::Release));

        let rec = selection_copied.get().expect("release records a copy");
        assert_eq!(rec.chars, 5, "five characters copied (\"row 1\")");
    }

    /// Select-to-copy on a transcript shorter than the viewport. Bottom-anchoring
    /// leaves a blank band above the first entry, yet a drag over the visible
    /// text must select that text and paint the highlight on the content rows,
    /// not the blank band. Without the top-pad offset the screen-row math treats
    /// row 0 as the first content line, so the drag resolves to the empty rows
    /// past the end, copies nothing, and highlights the wrong row.
    #[test]
    fn select_to_copy_on_short_bottom_anchored_transcript() {
        let chat = chat_with_notices(2);
        let mut view = transcript_view(&chat);
        let ctx = draw_ctx(40, 10);
        let bg = view.styles.selection_bg;

        // A fresh short transcript bottom-anchors: two 2-row notices sit at
        // screen rows 6..=9, leaving rows 0..=5 blank. " row 0" lands at row 6.
        let _ = view.draw(&ctx);
        let content_row: i16 = 6;

        // Press past the leading space, then drag across " row 0".
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(1, content_row, mouse::Type::Press));
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(6, content_row, mouse::Type::Drag));

        let sel = view.selection.expect("drag anchors a selection");
        assert_eq!(
            sel.anchor.entry,
            entry_id(&chat, 0),
            "anchored on the first entry, not a spurious one past the end",
        );
        assert_eq!((sel.anchor.line, sel.anchor.col), (0, 1));
        assert_eq!((sel.caret.line, sel.caret.col), (0, 6));

        // Release copies the visible text, not the blank band.
        let mut ec = EventContext::new();
        view.handle_event(&mut ec, &mouse(6, content_row, mouse::Type::Release));
        let copied = ec.cmds.iter().find_map(|cmd| match cmd {
            vaxis::vxfw::Command::CopyToClipboard(text) => Some(text.clone()),
            _ => None,
        });
        assert_eq!(copied.as_deref(), Some("row 0"), "copied the visible text");

        // The highlight lands on the content row; the blank top band is untouched.
        let surface = view.draw(&ctx);
        let grid = crate::test_support::flatten(&surface);
        assert_eq!(
            highlighted_rows(&grid, bg),
            vec![6],
            "highlight on the content row, not the blank band",
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
