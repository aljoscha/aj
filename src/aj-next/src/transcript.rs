//! The transcript view: a scrolling `ListView` over the active
//! transcript's entries.
//!
//! One list item per [`Entry`], built on demand from the shared
//! [`ChatState`] via `Source::Builder`, so a long transcript only
//! materializes the visible rows each frame. The per-entry widget
//! builder ([`build_entry_widget`]) is shared with the sub-agent
//! box, which lays the same widgets out inside its own frame.

use std::cell::RefCell;
use std::rc::Rc;

use aj_app::chat::{ChatState, Entry, EntryKind, NoticeLevel, UserEntry};
use aj_app::footer::format_tokens;
use aj_app::theme::{ColorMode, Theme, ThemeBg, ThemeColor, ThemeRgb, rgb_to_256};
use aj_models::types::AssistantContent;
use aj_tools::sanitize_terminal_output;
use vaxis::cell::{Cell, Character, Color, Style};
use vaxis::mouse;
use vaxis::vxfw::{
    Builder, DrawContext, Event, EventContext, ListView, RelativePoint, RichText, ScrollBars,
    Source, SubSurface, Surface, TextSpan, Widget, WidgetRef,
};

use crate::bubble::Bubble;
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
    pub(crate) diff_context: Style,
    /// Tool-bubble tints per visual status.
    pub(crate) tool_pending_bg: Color,
    pub(crate) tool_success_bg: Color,
    pub(crate) tool_error_bg: Color,
    /// The user-message bubble tint.
    pub(crate) user_message_bg: Color,
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
            diff_add: fg(ThemeColor::ToolDiffAdded),
            diff_remove: fg(ThemeColor::ToolDiffRemoved),
            diff_context: fg(ThemeColor::ToolDiffContext),
            tool_pending_bg: bg(ThemeBg::ToolPendingBg),
            tool_success_bg: bg(ThemeBg::ToolSuccessBg),
            tool_error_bg: bg(ThemeBg::ToolErrorBg),
            user_message_bg: bg(ThemeBg::UserMessageBg),
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

/// Lazily builds one row widget per transcript entry of the active
/// view. Shared with the [`ListView`] it feeds.
struct EntryBuilder {
    chat: Rc<RefCell<ChatState>>,
    styles: Rc<TranscriptStyles>,
}

impl Builder for EntryBuilder {
    fn item_at_idx(&self, idx: usize, _cursor: usize) -> Option<WidgetRef> {
        let chat = self.chat.borrow();
        let entry = chat.transcript(chat.active_view())?.entries().get(idx)?;
        // Widgets read the display flags at build time, so flipping a
        // flag only needs a redraw.
        Some(
            match build_entry_widget(entry, &chat, &self.styles, false) {
                EntryWidget::Bubble(b) => Rc::new(RefCell::new(b)),
                EntryWidget::Rich(r) => Rc::new(RefCell::new(r)),
                EntryWidget::SubAgent(b) => Rc::new(RefCell::new(b)),
            },
        )
    }
}

/// A built per-entry widget. One enum instead of a boxed trait
/// object so the `ListView` path can wrap each concrete type in its
/// own `WidgetRef` (the unsize coercion needs the concrete type).
pub(crate) enum EntryWidget {
    Bubble(Bubble),
    Rich(RichText),
    SubAgent(SubAgentBox),
}

impl EntryWidget {
    /// Erase to a boxed widget, for the sub-agent box's child list.
    pub(crate) fn into_boxed(self) -> Box<dyn Widget> {
        match self {
            EntryWidget::Bubble(b) => Box::new(b),
            EntryWidget::Rich(r) => Box::new(r),
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
        _ => EntryWidget::Rich(RichText::new(entry_spans(
            entry,
            chat.hide_thinking_block,
            chat.tools_expanded,
            styles,
        ))),
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

/// Build the styled spans for one entry, ending in a blank spacer row
/// so consecutive entries don't visually collide.
fn entry_spans(
    entry: &Entry,
    hide_thinking: bool,
    tools_expanded: bool,
    styles: &TranscriptStyles,
) -> Vec<TextSpan> {
    let span = |text: String, style: Style| TextSpan {
        text,
        style,
        ..TextSpan::default()
    };
    let mut spans = match &entry.kind {
        // User entries render through the bubble widget (see
        // `build_entry_widget`). This arm only exists so the match
        // stays total.
        EntryKind::User(_) => Vec::new(),
        EntryKind::Assistant(a) => {
            let mut spans = Vec::new();
            for block in &a.message.content {
                let block_span = match block {
                    AssistantContent::Text(t) => span(t.text.clone(), styles.text),
                    AssistantContent::Thinking(t) if t.redacted => span(
                        format!("[Redacted thinking: {}]", t.thinking),
                        styles.thinking,
                    ),
                    AssistantContent::Thinking(_) if hide_thinking => {
                        span("Thinking…".to_string(), styles.thinking)
                    }
                    AssistantContent::Thinking(t) => {
                        span(format!("Thinking: {}", t.thinking), styles.thinking)
                    }
                    // Tool calls render as their own `Tool` transcript
                    // entries, so the inline block would duplicate them.
                    AssistantContent::ToolCall(_) => continue,
                };
                if !spans.is_empty() {
                    spans.push(span("\n\n".to_string(), styles.text));
                }
                spans.push(block_span);
            }
            spans
        }
        // Tool entries render through the `ToolCell` bubble and
        // sub-agent entries through the `SubAgentBox` (see
        // `build_entry_widget`). The `SubAgent` arm is only reachable
        // as the nested-inside-a-box fallback, which can't occur live
        // (sub-agents don't spawn sub-agents), so a dim stub is
        // enough.
        EntryKind::Tool(_) => Vec::new(),
        EntryKind::SubAgent(s) => vec![span(format!("[sub-agent {}]", s.child), styles.dim)],
        // The durable record of a context compaction: a dim header
        // stating the token delta, expandable to the generated
        // summary. Folding rides the session-wide `tools_expanded`
        // flag, the same one tool bodies honor, so a compaction
        // summary expands and collapses together with tool results
        // under one keystroke.
        EntryKind::Compaction(c) => {
            // One-column inset like the notice rows, no vertical
            // padding of its own (the trailing spacer supplies the
            // gap).
            let mut header = format!(" {}", compaction_header(c.tokens_before, c.tokens_after));
            if !tools_expanded && !c.summary.is_empty() {
                let key = EXPAND_KEY_LABEL.as_str();
                header.push_str(&format!(" ({key} to expand)"));
            }
            let mut spans = vec![span(header, styles.dim)];
            if tools_expanded && !c.summary.is_empty() {
                // Markdown rendering is deferred, so the summary
                // shows as plain wrapped text, separated from the
                // header by one blank row.
                spans.push(span("\n\n".to_string(), styles.text));
                spans.push(span(c.summary.clone(), styles.text));
            }
            spans
        }
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
    /// While true, every draw pins the viewport to the bottom so a
    /// streaming turn stays in view. Wheel-up and thumb drags
    /// disengage, a scroll that lands back at the bottom re-engages
    /// (Spec E section 1).
    follow_tail: bool,
}

impl TranscriptView {
    pub fn new(chat: Rc<RefCell<ChatState>>, theme: &Theme) -> TranscriptView {
        let styles = Rc::new(TranscriptStyles::from_theme(theme));
        let builder = EntryBuilder {
            chat: Rc::clone(&chat),
            styles: Rc::clone(&styles),
        };
        let mut list = ListView::new(Source::Builder(Box::new(builder)));
        // Free-scroll mode: no item cursor while the editor owns the
        // keyboard. Transcript-focus mode arrives in a later phase.
        list.draw_cursor = false;
        let mut bars = ScrollBars::new(list);
        bars.draw_horizontal_scrollbar = false;
        apply_scrollbar_thumbs(&mut bars, &styles);
        let list = Rc::clone(&bars.view);
        TranscriptView {
            chat,
            list,
            bars,
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
        self.follow_tail = true;
    }

    /// Rebuild the transcript's styles from a fresh palette, for a
    /// runtime theme swap. Replaces the row builder (so the per-entry
    /// widgets, which are rebuilt every frame, pick up the new colors)
    /// and re-applies the scrollbar thumb tints. Scroll position is
    /// left untouched, so a reload doesn't jump the viewport.
    pub(crate) fn set_styles(&mut self, styles: Rc<TranscriptStyles>) {
        let builder = EntryBuilder {
            chat: Rc::clone(&self.chat),
            styles: Rc::clone(&styles),
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
    use aj_app::chat::{
        AssistantEntry, CompactionEntry, EntryId, NoticeEntry, Transcript, UserEntry,
    };
    use aj_models::types::{
        AssistantContent, AssistantMessage, StopReason, TextContent, ThinkingContent, UserContent,
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

    #[test]
    fn user_entry_spans_are_empty_the_bubble_renders_it() {
        // User entries render through `build_user_bubble`, so the
        // span path only carries the spacer.
        let t = transcript_with(EntryKind::User(UserEntry {
            content: vec![UserContent::text("hello")],
            collapsible: false,
        }));
        let spans = entry_spans(&t.entries()[0], false, false, &styles());
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
                AssistantContent::Text(TextContent {
                    text: "answer\n".into(),
                    text_signature: None,
                }),
            ]),
            finalized: true,
        }));
        let spans = entry_spans(&t.entries()[0], false, false, &styles());
        // Trailing newline of the last block is normalized so the
        // spacer contributes exactly one blank row.
        assert_eq!(joined(&spans), "Thinking: pondering\n\nanswer\n\n");
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
        let spans = entry_spans(&t.entries()[0], true, false, &styles());
        assert_eq!(joined(&spans), "Thinking…\n\n");
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
        let spans = entry_spans(&t.entries()[0], false, false, &styles());
        assert_eq!(joined(&spans), "[Redacted thinking: ]\n\n");
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
            let spans = entry_spans(&t.entries()[0], false, false, &s);
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
        let spans = entry_spans(&t.entries()[0], false, false, &styles());
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
        let spans = entry_spans(&t.entries()[0], false, false, &styles());
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
        let spans = entry_spans(&t.entries()[0], false, false, &styles());
        let text = joined(&spans);
        assert!(text.contains("Context compacted"), "{text:?}");
        assert!(text.contains("(freed 75%)"), "{text:?}");
        assert!(text.contains("(Alt+O to expand)"), "{text:?}");
        assert!(!text.contains("secret summary body"), "{text:?}");
        assert!(text.starts_with(' '), "one-column inset: {text:?}");
        assert_eq!(spans[0].style, styles().dim, "dim header");
    }

    /// With nothing to reveal there is nothing to advertise.
    #[test]
    fn collapsed_compaction_without_summary_has_no_expand_hint() {
        let t = compaction("");
        let spans = entry_spans(&t.entries()[0], false, false, &styles());
        assert!(!joined(&spans).contains("to expand"), "{spans:?}");
    }

    #[test]
    fn expanded_compaction_shows_summary_after_a_blank_row() {
        let t = compaction("the full summary body");
        let spans = entry_spans(&t.entries()[0], false, true, &styles());
        let text = joined(&spans);
        assert!(!text.contains("to expand"), "{text:?}");
        assert!(
            text.contains("tokens (freed 75%)\n\nthe full summary body"),
            "blank row between header and body: {text:?}",
        );
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
        use std::sync::Arc;

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
}
