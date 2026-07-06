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
use aj_app::markdown::{Emphasis, RenderOpts};
use aj_app::theme::{ColorMode, Theme, ThemeBg, ThemeColor, ThemeRgb, rgb_to_256};
use aj_models::types::AssistantContent;
use aj_tools::sanitize_terminal_output;
use serde_json::Value;
use vaxis::cell::{Cell, Character, Color, Style};
use vaxis::mouse;
use vaxis::vxfw::{
    Builder, DrawContext, Event, EventContext, ListView, RelativePoint, RichText, ScrollBars,
    Source, SubSurface, Surface, TextSpan, Widget, WidgetRef,
};

use crate::bubble::Bubble;
use crate::markdown_view::{MarkdownSegment, MarkdownStyles, MarkdownView};
use crate::subagent_box::{SubAgentBox, build_subagent_box};
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
    pub(crate) diff_add: Style,
    pub(crate) diff_remove: Style,
    /// Tool-bubble tints per visual status.
    pub(crate) tool_pending_bg: Color,
    pub(crate) tool_success_bg: Color,
    pub(crate) tool_error_bg: Color,
    /// The user-message bubble tint.
    pub(crate) user_message_bg: Color,
    /// Foreground mapper for markdown span roles, consumed by
    /// [`MarkdownView`]. Rebuilt from the theme here so a runtime swap
    /// re-tints markdown through the same `set_styles` path.
    pub(crate) markdown: MarkdownStyles,
    /// Whether markdown links emit OSC-8 hyperlinks. See
    /// [`TERMINAL_HYPERLINKS`].
    pub(crate) hyperlinks: bool,
}

/// Whether the markdown renderer emits OSC-8 hyperlinks.
///
/// vaxis's `Capabilities` surfaces no hyperlink probe, so there is nothing to
/// read from `app.vaxis().caps`: we optimistically enable OSC-8. vaxis writes
/// the escape unconditionally and terminals that lack support ignore the
/// bytes. TODO(aljoscha): thread a real capability once vaxis detects
/// hyperlink support, wiring it through `from_theme` the way `ColorMode` is.
const TERMINAL_HYPERLINKS: bool = true;

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
            diff_add: fg(ThemeColor::ToolDiffAdded),
            diff_remove: fg(ThemeColor::ToolDiffRemoved),
            tool_pending_bg: bg(ThemeBg::ToolPendingBg),
            tool_success_bg: bg(ThemeBg::ToolSuccessBg),
            tool_error_bg: bg(ThemeBg::ToolErrorBg),
            user_message_bg: bg(ThemeBg::UserMessageBg),
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
/// `tools_expanded`, `hide_thinking_block`, the active view) are handled by
/// clearing the whole cache when they change rather than folding them into
/// every fingerprint (see [`TranscriptView::draw`] and
/// [`TranscriptView::set_styles`]). Width is a per-slot key.
///
/// Storing surfaces is safe today: no transcript entry participates in event
/// dispatch and the list draws no cursor, so replaying a stored surface
/// (whose `widget` stamp is `None`, then re-stamped onto the outer
/// [`CachingEntry`] by `draw_widget`) does not break hit-testing.
/// NOTE(aljoscha): a later transcript-focus mode would make entries
/// interactive, at which point the wrapper must forward events to the real
/// widget or the cache must store widgets for those kinds.
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
}

impl Builder for EntryBuilder {
    fn item_at_idx(&self, idx: usize, _cursor: usize) -> Option<WidgetRef> {
        let chat = self.chat.borrow();
        let agent = chat.active_view();
        let entry = chat.transcript(agent)?.entries().get(idx)?;
        // Cheap, layout-free fingerprint of the live entry. The wrapper's
        // draw compares it against the cached slot to decide hit vs miss.
        let fingerprint = entry_fingerprint(entry, &chat);
        Some(Rc::new(RefCell::new(CachingEntry {
            cache: Rc::clone(&self.cache),
            chat: Rc::clone(&self.chat),
            styles: Rc::clone(&self.styles),
            agent,
            entry_id: entry.id,
            fingerprint,
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
            let mut widget = build_entry_widget(entry, &chat, &self.styles, false).into_boxed();
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
/// `hide_thinking_block`, the active view, the theme, the draw width) are NOT
/// hashed here: the cache clears wholesale when they change, and width is a
/// per-slot key.
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
pub(crate) fn build_entry_widget(
    entry: &Entry,
    chat: &ChatState,
    styles: &TranscriptStyles,
    nested: bool,
) -> EntryWidget {
    match &entry.kind {
        EntryKind::Tool(tool) => EntryWidget::Bubble(build_tool_cell(
            tool,
            chat.tasks(),
            chat.tools_expanded,
            styles,
        )),
        EntryKind::User(user) => {
            EntryWidget::Bubble(build_user_bubble(user, chat.tools_expanded, styles))
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
            styles,
        )),
        EntryKind::Compaction(c) => {
            EntryWidget::Markdown(build_compaction_markdown(c, chat.tools_expanded, styles))
        }
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
fn build_user_bubble(user: &UserEntry, expanded: bool, styles: &TranscriptStyles) -> Bubble {
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
    Bubble::entry(spans, Some(styles.user_message_bg), styles.text)
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
    styles: &TranscriptStyles,
) -> MarkdownView {
    let text_opts = RenderOpts {
        hyperlinks: styles.hyperlinks,
        default_emphasis: Emphasis::default(),
    };
    // Thinking prose renders italic by default, the same emphasis `aj` applies
    // to a thinking block's markdown.
    let thinking_opts = RenderOpts {
        hyperlinks: styles.hyperlinks,
        default_emphasis: Emphasis {
            italic: true,
            ..Emphasis::default()
        },
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
    /// Last-seen session-wide render inputs. When any of these changes the
    /// whole cache is cleared, since they are not part of any per-entry
    /// fingerprint.
    last_globals: GlobalRenderInputs,
    /// While true, every draw pins the viewport to the bottom so a
    /// streaming turn stays in view. Wheel-up and thumb drags
    /// disengage, a scroll that lands back at the bottom re-engages
    /// (Spec E section 1).
    follow_tail: bool,
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
}

impl GlobalRenderInputs {
    fn read(chat: &ChatState) -> GlobalRenderInputs {
        GlobalRenderInputs {
            active_view: chat.active_view(),
            tools_expanded: chat.tools_expanded,
            hide_thinking_block: chat.hide_thinking_block,
        }
    }
}

impl TranscriptView {
    pub fn new(chat: Rc<RefCell<ChatState>>, theme: &Theme) -> TranscriptView {
        let styles = Rc::new(TranscriptStyles::from_theme(theme));
        let cache = Rc::new(RefCell::new(EntryRenderCache::new()));
        let builder = EntryBuilder {
            chat: Rc::clone(&chat),
            styles: Rc::clone(&styles),
            cache: Rc::clone(&cache),
        };
        let mut list = ListView::new(Source::Builder(Box::new(builder)));
        // Free-scroll mode: no item cursor while the editor owns the
        // keyboard. Transcript-focus mode arrives in a later phase.
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
            last_globals,
            follow_tail: true,
        }
    }

    /// Re-engage follow-tail so the next draw pins the viewport to the
    /// bottom. Used on a session rebuild: the view's `chat` cell keeps its
    /// identity across the swap (the outer loop overwrites its contents in
    /// place), so the fresh session's transcript opens at the tail rather
    /// than wherever the previous session was scrolled. The draw path
    /// refreshes `item_count` before scrolling, so we needn't touch the
    /// list's scroll offset here.
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
        self.cache.borrow_mut().clear();
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
        let builder = EntryBuilder {
            chat: Rc::clone(&self.chat),
            styles: Rc::clone(&styles),
            cache: Rc::clone(&self.cache),
        };
        self.list.borrow_mut().children = Source::Builder(Box::new(builder));
        apply_scrollbar_thumbs(&mut self.bars, &styles);
    }

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
            self.last_globals = globals;
        }
        let count = {
            let chat = self.chat.borrow();
            chat.transcript(chat.active_view())
                .map(|t| t.entries().len())
                .unwrap_or(0)
        };
        {
            let mut list = self.list.borrow_mut();
            // The builder has no inherent end-of-list knowledge worth
            // walking for, so refresh the exact count every draw. It also
            // makes `scroll_to_bottom` cheap (no builder walk).
            list.item_count = Some(u32::try_from(count).expect("entry count fits u32"));
            if self.follow_tail {
                list.scroll_to_bottom();
            }
        }
        // The bars draw the list one column narrower and add the thumb
        // when the reconciled scroll says the transcript overflows.
        let bars_surface = self.bars.draw(ctx);
        // The draw reconciled any pending wheel scroll, so "we are at
        // the bottom" is now accurate. Landing there re-engages
        // follow-tail.
        if self.list.borrow().is_at_bottom() {
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
                if m.button == mouse::Button::WheelUp {
                    self.follow_tail = false;
                }
                self.list.borrow_mut().handle_event(ctx, event);
            }
            // The bars cancel an in-flight drag when the mouse leaves.
            Event::MouseLeave => self.bars.handle_event(ctx, event),
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
        let mut view = build_assistant_markdown(a, hide_thinking, &styles());
        let surface = view.draw(&crate::test_support::draw_ctx(width, None));
        crate::test_support::rows(&surface)
    }

    /// Draw the compaction entry's `MarkdownView` at `width` and return its
    /// composited rows plus the header's (first visible cell's) style.
    fn compaction_view_rows(t: &Transcript, expanded: bool, width: u16) -> (Vec<String>, Style) {
        let EntryKind::Compaction(c) = &t.entries()[0].kind else {
            panic!("expected a compaction entry");
        };
        let mut view = build_compaction_markdown(c, expanded, &styles());
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
        let mut bubble = build_user_bubble(user, expanded, &styles());
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
        let mut bubble = build_user_bubble(&user, false, &s);
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
        let mut bubble = build_user_bubble(&user, false, &s);
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
        TranscriptView::new(Rc::clone(chat), &theme)
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
        EntryBuilder {
            chat: Rc::clone(chat),
            styles: Rc::new(styles()),
            cache: Rc::new(RefCell::new(EntryRenderCache::new())),
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

    /// Draw entry `idx` of `agent` with a fresh, uncached widget: the
    /// reference a cached render must match byte-for-byte.
    fn draw_uncached(builder: &EntryBuilder, agent: AgentId, idx: usize, width: u16) -> Surface {
        let chat = builder.chat.borrow();
        let entry = &chat.transcript(agent).expect("transcript").entries()[idx];
        let mut widget = build_entry_widget(entry, &chat, &builder.styles, false).into_boxed();
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
}
