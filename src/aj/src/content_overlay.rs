//! Read-only content overlays: help, auth status, and session info.
//!
//! A [`ContentOverlay`] is a scrollable, non-interactive list of text
//! rows shown inside an [`OverlayWindow`]. Esc or Enter close it
//! (returning to the parent overlay or the editor). The body navigates
//! like a pager: Up/Down (and Ctrl+P/Ctrl+N) scroll a single line and
//! PgUp/PgDn scroll a viewport-scaled page, while Home/End jump straight
//! to the first and last line rather than scrolling. Every other key is
//! swallowed so nothing leaks to the
//! layout behind the modal. Each row is a list of styled spans ([`Row`])
//! the host builds from the shared `aj_app` data, so the one widget backs
//! all three read-only overlays.
//!
//! The usage row builder ([`usage_rows`]) also lives here, but the usage
//! page is interactive ([`crate::usage_overlay`]) and reuses only the row
//! layout, not the [`ContentOverlay`] widget.
//!
//! Async overlays (auth, session info) open showing a single
//! "Loading…" row and are refilled through the [`ListView`] handle
//! [`open_content_overlay`] returns once the host's fetch lands. That
//! keeps the fetch off the open path so a slow network probe never
//! blocks the overlay from appearing.

use std::cell::RefCell;
use std::rc::Rc;

use aj_app::auth::ProviderAuthStatus;
use aj_app::commands::COMMANDS;
use aj_app::keybindings::{
    ACTION_AGENT_PICKER, ACTION_CHAT_PAGE_DOWN, ACTION_CHAT_PAGE_UP, ACTION_CHAT_SCROLL_BOTTOM,
    ACTION_CHAT_SCROLL_TOP, ACTION_CLIPBOARD_PASTE_IMAGE, ACTION_COPY_MESSAGE, ACTION_DEQUEUE,
    ACTION_HISTORY_OPEN, ACTION_PALETTE_OPEN, ACTION_SUBMIT_STEERING, ACTION_THINKING_TOGGLE,
    ACTION_TOOLS_EXPAND, ACTION_TRANSCRIPT_FOCUS, AJ_KEYBINDINGS, action_shortcut,
};
use aj_app::theme::{Theme, ThemeColor};
use aj_app::usage::{ProviderUsageStatus, UsageOutcome, format_window_status, now_unix_ms};
use aj_session::SessionStats;
use vaxis::cell::{Segment, Style};
use vaxis::key::{Key, Modifiers};
use vaxis::vxfw::{
    DrawContext, Event, EventContext, ListView, OverlayWindow, RelativePoint, RichText, ScrollBars,
    Source, SubSurface, Surface, TextArea, Widget, WidgetRef, to_widget_ref,
};

use crate::overlay::{
    OpenOverlay, OverlayChrome, OverlayPlacement, OverlayStack, close_top, subtitle_close,
};
use crate::transcript::vaxis_color;

/// A single content-overlay row: styled spans laid out as one line.
///
/// A plain row is one default-styled span. Because [`RichText::new`]'s
/// layout defaults match [`Text`](vaxis::vxfw::Text)'s, a plain row draws
/// exactly like a plain-string row would, so converting the plain builders
/// to this model is appearance-preserving.
pub(crate) type Row = Vec<Segment>;

/// A single default-styled span carrying `text`.
pub(crate) fn plain(text: impl Into<String>) -> Row {
    vec![span(text, Style::default())]
}

/// A styled span for one column of a row.
fn span(text: impl Into<String>, style: Style) -> Segment {
    Segment {
        text: text.into(),
        style,
        ..Segment::default()
    }
}

/// Column tints for the read-only content pages, resolved once from the
/// theme like [`OverlayChrome`]. `muted` tints the provider-id and secondary
/// detail columns, matching `aj`'s auth page. `heading` colors the help
/// page's section headings.
#[derive(Clone, Copy)]
pub(crate) struct ContentStyles {
    pub(crate) muted: Style,
    pub(crate) heading: Style,
}

impl ContentStyles {
    pub(crate) fn from_theme(theme: &Theme) -> ContentStyles {
        let mode = theme.color_mode();
        let fg = |token: ThemeColor| Style {
            fg: vaxis_color(theme.fg_color(token), mode),
            ..Style::default()
        };
        ContentStyles {
            muted: fg(ThemeColor::Muted),
            // `Accent` is the overlay title's emphasis token (a lavender in
            // both bundled themes), so a section heading reads with the same
            // colored emphasis as the window title above it, drawn bold on
            // top. We avoid `MdHeading` here: it is empty in the bundled
            // palettes, so it would render bold-only with no color and miss
            // the spec's colored-heading requirement.
            heading: Style {
                bold: true,
                ..fg(ThemeColor::Accent)
            },
        }
    }
}

/// A scrollable, read-only list of text rows.
///
/// Focus sits on this widget while it is the top overlay, so it
/// intercepts every key in its capturing phase: Esc/Enter close (via
/// [`Self::on_close`]), the arrow and page keys scroll the body (a line
/// or a page at a time) while Home/End jump to the first and last line,
/// and everything else is consumed so it can't reach the base layout.
pub(crate) struct ContentOverlay {
    /// The row list, shared with `bars` (which draws it) and handed back
    /// by [`open_content_overlay`] so the host can refill an async
    /// overlay's rows after the initial "Loading…" state.
    list: Rc<RefCell<ListView>>,
    bars: Rc<RefCell<ScrollBars<ListView>>>,
    /// Tint applied to the scroll-bar thumb on each draw. Defaults to
    /// [`Style::default`] so a bare [`ContentOverlay::new`] draws an
    /// untinted thumb. [`open_content_overlay`] sets it to the chrome's
    /// Muted thumb style.
    thumb_style: Style,
    /// Closes this overlay and restores focus to the parent. Runs inside
    /// key dispatch, where the live [`EventContext`] can move focus.
    pub(crate) on_close: Option<Box<dyn FnMut(&mut EventContext)>>,
}

impl ContentOverlay {
    pub(crate) fn new(rows: Vec<Row>) -> ContentOverlay {
        let mut list = ListView::new(Source::Slice(row_widgets(&rows)));
        list.item_count = Some(u32::try_from(rows.len()).expect("row count fits u32"));
        // No visible cursor: the body scrolls as a document via
        // `scroll_lines`, so there is no item cursor to render.
        list.draw_cursor = false;
        let bars = ScrollBars::new(list);
        bars.borrow_mut().draw_horizontal_scrollbar = false;
        let list = Rc::clone(&bars.borrow().view);
        ContentOverlay {
            list,
            bars,
            thumb_style: Style::default(),
            on_close: None,
        }
    }

    /// The shared row list, for the host to refill after an async fetch.
    pub(crate) fn list_handle(&self) -> Rc<RefCell<ListView>> {
        Rc::clone(&self.list)
    }

    /// Set the tint applied to the scroll-bar thumb on each draw.
    pub(crate) fn set_thumb_style(&mut self, style: Style) {
        self.thumb_style = style;
    }
}

impl Widget for ContentOverlay {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // Wrap the bars in an opaque full-size surface so a shorter
        // refill can't leave stale cells from a taller previous frame.
        let mut surface = Surface::with_size(ctx.max.size());
        // Apply the tint per-draw for parity with the other lists. The thumb
        // style is set once from the chrome at open and never mutated after, so
        // this is parity, not staleness prevention.
        let bars_surface = {
            let mut bars = self.bars.borrow_mut();
            crate::scroll::apply_thumb_style(&mut bars, self.thumb_style);
            bars.draw(ctx)
        };
        surface.children.push(SubSurface {
            origin: RelativePoint { row: 0, col: 0 },
            surface: bars_surface,
            z_index: 0,
        });
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::KeyPress(key) = event else {
            return;
        };
        let empty = Modifiers::empty();
        let ctrl = Modifiers::CTRL;
        // Esc and Enter both close: a read-only view has nothing to
        // confirm, so Enter is just a second "dismiss" key (matching
        // `aj`'s read-only overlays).
        if key.matches(Key::ESCAPE, empty) || key.matches(Key::ENTER, empty) {
            if let Some(cb) = self.on_close.as_mut() {
                cb(ctx);
            }
            ctx.consume_and_redraw();
            return;
        }
        // Document scroll by line: `scroll_lines` moves the viewport
        // immediately and clamps at both ends, unlike cursor-item nav, which
        // only shifts the viewport once the hidden cursor leaves it (so the
        // first viewport-worth of presses looked dead).
        // Read the viewport under a short immutable borrow that drops at the
        // end of the statement, before the `borrow_mut` calls below.
        let page = crate::scroll::page_scroll_lines(self.list.borrow().viewport_height());
        if key.matches(Key::DOWN, empty) || key.matches(u32::from('n'), ctrl) {
            self.list.borrow_mut().scroll_lines(1);
        } else if key.matches(Key::UP, empty) || key.matches(u32::from('p'), ctrl) {
            self.list.borrow_mut().scroll_lines(-1);
        } else if key.matches(Key::PAGE_DOWN, empty) {
            self.list.borrow_mut().scroll_lines(page);
        } else if key.matches(Key::PAGE_UP, empty) {
            self.list.borrow_mut().scroll_lines(-page);
        } else if key.matches(Key::HOME, empty) {
            // Pin the scroll to the very first line, matching the transcript's
            // editor-mode Home.
            self.list.borrow_mut().jump_to_item(0);
        } else if key.matches(Key::END, empty) {
            self.list.borrow_mut().scroll_to_bottom();
        }
        // Read-only: swallow every key so none reaches the base layout.
        ctx.consume_and_redraw();
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// Build the list-row widgets for a set of rows.
fn row_widgets(rows: &[Row]) -> Vec<WidgetRef> {
    rows.iter()
        .map(|r| {
            let text: WidgetRef = Rc::new(RefCell::new(RichText::new(r.clone())));
            text
        })
        .collect()
}

/// Replace an open overlay's rows, for an async overlay whose fetch has
/// landed. Resets the scroll to the top so the filled content reads from
/// its first row.
pub(crate) fn set_rows(list: &Rc<RefCell<ListView>>, rows: Vec<Row>) {
    let mut list = list.borrow_mut();
    list.item_count = Some(u32::try_from(rows.len()).expect("row count fits u32"));
    list.children = Source::Slice(row_widgets(&rows));
    list.jump_to_item(0);
}

/// Push a read-only content overlay showing `rows` on top of `stack`,
/// keeping any parent overlay underneath (so Esc returns to it), and
/// move focus onto it.
///
/// Returns the row-list handle so the host can [`set_rows`] later. An
/// async overlay passes a `["Loading…"]` seed and fills it when its
/// fetch completes.
pub(crate) fn open_content_overlay(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    title: &str,
    rows: Vec<Row>,
    ctx: &mut EventContext,
) -> Rc<RefCell<ListView>> {
    let content = Rc::new(RefCell::new(ContentOverlay::new(rows)));
    // The chrome's Muted thumb style is applied at open time (and again on
    // reopen), like the window border and title styles below, rather than
    // live-reskinned.
    content
        .borrow_mut()
        .set_thumb_style(chrome.select.scrollbar_thumb);
    let list = content.borrow().list_handle();
    {
        let stack_for_close = Rc::clone(stack);
        let editor_for_close = Rc::clone(editor);
        content.borrow_mut().on_close = Some(Box::new(move |ctx| {
            close_top(&stack_for_close, ctx, &editor_for_close);
        }));
    }
    // The window's child and the focus target are the same widget: keys
    // route to the ContentOverlay while the OverlayWindow supplies the
    // border and title chrome around it.
    let focus: WidgetRef = to_widget_ref(Rc::clone(&content));
    let mut window = OverlayWindow::new(title, to_widget_ref(content));
    // The close hint resolves through the shared keybinding data (Spec F):
    // Esc's label from `format_keybinding`, the close-all label from the
    // keymap action. The Esc/Enter *handling* stays a fixed `ContentOverlay`
    // convention (see the NOTE in `crate::overlay`).
    window.subtitle = subtitle_close();
    window.border_style = chrome.border;
    window.title_style = chrome.title;
    window.subtitle_style = chrome.subtitle;
    stack.borrow_mut().push(OpenOverlay {
        widget: to_widget_ref(Rc::new(RefCell::new(window))),
        focus: Rc::clone(&focus),
        placement: OverlayPlacement::Large,
    });
    ctx.request_focus(focus);
    ctx.redraw = true;
    list
}

// ============================================================================
// Row builders
// ============================================================================

/// The single-row "Loading…" seed an async overlay opens with.
pub(crate) fn loading_rows() -> Vec<Row> {
    vec![plain("Loading\u{2026}")]
}

/// The compose-time global chords listed under section 1 (editor
/// shortcuts). These are the app-level chords a user can fire while the
/// editor is focused (open the palette, paste an image, toggle thinking
/// or tool output, recall or steer a message, open the pickers).
///
/// Resolved through [`action_shortcut`] at render time, never a
/// literal, so a rebind relabels the row.
const COMPOSE_GLOBAL_ACTIONS: &[&str] = &[
    ACTION_PALETTE_OPEN,
    ACTION_CLIPBOARD_PASTE_IMAGE,
    ACTION_THINKING_TOGGLE,
    ACTION_TOOLS_EXPAND,
    ACTION_HISTORY_OPEN,
    ACTION_AGENT_PICKER,
    ACTION_SUBMIT_STEERING,
    ACTION_DEQUEUE,
];

/// The chat-scroll and transcript-navigation chords listed under section 2.
/// `ACTION_COPY_MESSAGE` lives in transcript-focus mode, so it belongs with
/// the transcript keys rather than the compose-time chords.
///
/// The overlay-management chord `ACTION_OVERLAY_CLOSE_ALL` and the
/// overlay-local chords (agent/history scope toggles, task kill, settings
/// clear, usage reset) are deliberately absent: each is surfaced by the
/// overlay it acts on (the close-all label rides every overlay subtitle),
/// not by this keymap reference.
const SCROLL_NAV_ACTIONS: &[&str] = &[
    ACTION_CHAT_PAGE_UP,
    ACTION_CHAT_PAGE_DOWN,
    ACTION_CHAT_SCROLL_TOP,
    ACTION_CHAT_SCROLL_BOTTOM,
    ACTION_TRANSCRIPT_FOCUS,
    ACTION_COPY_MESSAGE,
];

/// One line of the help page before layout: a colored section heading, a
/// muted sub-group label, a key/description entry, or a spacer.
enum HelpLine {
    /// Top-level section heading, drawn in the heading color.
    Heading(&'static str),
    /// Sub-group label within a section (editor chord group, or command
    /// category), drawn muted.
    Group(String),
    /// A key/label column and its description.
    Entry { key: String, desc: String },
    /// A visible blank line separating groups and sections.
    Blank,
}

/// The `(resolved key label, description)` for a global action, resolving
/// the label through [`action_shortcut`] so it tracks a rebind.
/// `None` for an action ID absent from [`AJ_KEYBINDINGS`].
fn global_chord(action_id: &str) -> Option<(String, &'static str)> {
    AJ_KEYBINDINGS
        .iter()
        .find(|(id, _, _)| *id == action_id)
        .map(|(id, _, desc)| (action_shortcut(id).unwrap_or_default(), *desc))
}

/// Section 1: the fixed editor editing chords (grouped by their own
/// `ChordDoc` group) plus the compose-time global chords.
///
/// The editor chords are the single source of truth exposed by
/// [`TextArea::bindings`], so their labels are the fixed `ChordDoc.keys`
/// literals (those chords are not rebindable). The global chords resolve
/// their labels through the keybinding data.
fn editor_shortcut_lines() -> Vec<HelpLine> {
    let mut lines = vec![HelpLine::Heading("Editor shortcuts")];
    // Bucket the editor chords by their display group, in first-appearance
    // order, so the reference reads Movement, Editing, Kill ring, Autocomplete.
    let mut groups: Vec<&'static str> = Vec::new();
    for chord in TextArea::bindings() {
        if !groups.contains(&chord.group) {
            groups.push(chord.group);
        }
    }
    for (i, group) in groups.iter().enumerate() {
        if i > 0 {
            lines.push(HelpLine::Blank);
        }
        lines.push(HelpLine::Group((*group).to_string()));
        for chord in TextArea::bindings().iter().filter(|c| c.group == *group) {
            lines.push(HelpLine::Entry {
                key: chord.keys.to_string(),
                desc: chord.description.to_string(),
            });
        }
    }
    lines.push(HelpLine::Blank);
    lines.push(HelpLine::Group("Global".to_string()));
    for id in COMPOSE_GLOBAL_ACTIONS {
        if let Some((key, desc)) = global_chord(id) {
            lines.push(HelpLine::Entry {
                key,
                desc: desc.to_string(),
            });
        }
    }
    lines
}

/// Section 2: the chat-scroll and transcript-navigation keys, with a
/// trailing descriptive row for the mouse wheel.
fn scroll_nav_lines() -> Vec<HelpLine> {
    let mut lines = vec![HelpLine::Heading("Scrolling & navigation")];
    for id in SCROLL_NAV_ACTIONS {
        if let Some((key, desc)) = global_chord(id) {
            lines.push(HelpLine::Entry {
                key,
                desc: desc.to_string(),
            });
        }
    }
    // The wheel is implicit pointer input, not a keybinding, so it carries
    // a plain label and its description says so, rather than a resolved chord.
    lines.push(HelpLine::Entry {
        key: "Mouse wheel".to_string(),
        desc: "Scroll the transcript (mouse input)".to_string(),
    });
    lines
}

/// Section 3: one row per [`COMMANDS`] entry, grouped by category, with the
/// bound shortcut resolved from each command's `action_id` and appended to
/// the description (matching the palette's shortcut resolution).
fn command_lines() -> Vec<HelpLine> {
    let mut lines = vec![HelpLine::Heading("Command palette commands")];
    // COMMANDS is authored in category order, so consecutive-run grouping
    // yields one labeled block per category.
    let mut current: Option<&str> = None;
    for cmd in COMMANDS {
        if current != Some(cmd.category) {
            if current.is_some() {
                lines.push(HelpLine::Blank);
            }
            lines.push(HelpLine::Group(cmd.category.to_string()));
            current = Some(cmd.category);
        }
        let mut desc = cmd.description.to_string();
        if let Some(short) = cmd.action_id.and_then(action_shortcut) {
            desc.push_str(&format!("  ({short})"));
        }
        lines.push(HelpLine::Entry {
            key: cmd.title.to_string(),
            desc,
        });
    }
    lines
}

/// Render one section's [`HelpLine`]s into styled rows, sizing the key
/// column to the widest entry key in that section so the description
/// column lines up within the section.
fn render_section(lines: &[HelpLine], styles: &ContentStyles) -> Vec<Row> {
    let key_w = lines
        .iter()
        .filter_map(|line| match line {
            HelpLine::Entry { key, .. } => Some(key.chars().count()),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| match line {
            HelpLine::Heading(text) => vec![span(*text, styles.heading)],
            HelpLine::Group(text) => vec![span(format!("  {text}"), styles.muted)],
            HelpLine::Entry { key, desc } => plain(format!("    {key:<key_w$}  {desc}")),
            // A single space, not the empty string: an empty `RichText` row
            // collapses to zero height in the `ListView` (see
            // `session_info_rows`).
            HelpLine::Blank => plain(" "),
        })
        .collect()
}

/// Help overlay rows: a grouped keymap reference in three sections, each
/// under a colored heading (editor shortcuts, scrolling & navigation, and
/// the command palette catalog).
///
/// Every displayed label is generated from authoritative data, never a
/// static snapshot: editor chords come from [`TextArea::bindings`], and
/// global-chord and command labels resolve through the keybinding data
/// (Spec F's hint-label rule), so a rebind flows through to the label.
pub(crate) fn help_rows(styles: &ContentStyles) -> Vec<Row> {
    let sections = [editor_shortcut_lines(), scroll_nav_lines(), command_lines()];
    let mut rows = Vec::new();
    for (i, section) in sections.iter().enumerate() {
        if i > 0 {
            rows.push(plain(" "));
        }
        rows.extend(render_section(section, styles));
    }
    rows
}

/// Auth-status rows: one per provider, its credential summary and any
/// secondary detail (e.g. token expiry).
///
/// Each row is three columns: the provider id in `styles.muted`, the summary
/// in the default style, and the optional detail in `styles.muted`. The
/// default-styled summary separates the two muted columns, so the detail
/// reads as its own field without the parentheses `aj` also omits. The id
/// column is right-aligned and the summary is padded to a shared width on
/// rows that carry a detail, so every detail value starts at the same column
/// and lines up.
pub(crate) fn auth_rows(statuses: &[ProviderAuthStatus], styles: &ContentStyles) -> Vec<Row> {
    if statuses.is_empty() {
        return vec![plain("No providers configured.")];
    }
    let id_w = statuses
        .iter()
        .map(|s| s.provider_id.chars().count())
        .max()
        .unwrap_or(0);
    // Only rows that carry a detail need a fixed summary column: padding
    // the summary to this width lands every detail value in the same
    // place. A detail-less row leaves its summary unpadded (no trailing
    // spaces), since it has no detail column to align to.
    let summary_w = statuses
        .iter()
        .filter(|s| s.detail.is_some())
        .map(|s| s.summary.chars().count())
        .max()
        .unwrap_or(0);
    statuses
        .iter()
        .map(|s| {
            let summary = if s.detail.is_some() {
                format!("  {summary:<summary_w$}", summary = s.summary)
            } else {
                format!("  {summary}", summary = s.summary)
            };
            let mut row = vec![
                span(format!("{id:>id_w$}", id = s.provider_id), styles.muted),
                span(summary, Style::default()),
            ];
            if let Some(detail) = &s.detail {
                row.push(span(format!("  {detail}"), styles.muted));
            }
            row
        })
        .collect()
}

/// The `(label, detail)` rows one provider contributes to the usage
/// page, in display order. The detail is the per-window status shown in
/// its own aligned column, `None` for rows that are just a label (notes,
/// "not configured", errors).
fn usage_status_rows(status: &ProviderUsageStatus, now_ms: i64) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    match &status.outcome {
        UsageOutcome::Usage(usage) => {
            if usage.windows.is_empty() && usage.notes.is_empty() && usage.reset_credits.is_none() {
                out.push(("no usage data reported".to_string(), None));
            }
            for window in &usage.windows {
                let desc = format_window_status(window.used, window.resets_at, now_ms);
                out.push((window.label.clone(), Some(desc)));
            }
            for note in &usage.notes {
                out.push((note.clone(), None));
            }
            if let Some(available) = usage.reset_credits {
                let desc = if available > 0 {
                    format!("{available} available")
                } else {
                    "no resets available".to_string()
                };
                out.push(("Rate-limit resets".to_string(), Some(desc)));
            }
        }
        UsageOutcome::Unsupported { reason } => {
            out.push((format!("usage not available \u{2014} {reason}"), None));
        }
        UsageOutcome::NotConfigured => out.push(("not configured".to_string(), None)),
        UsageOutcome::NoSource => out.push(("usage reporting not supported".to_string(), None)),
        UsageOutcome::Error(err) => out.push((format!("error: {err}"), None)),
    }
    out
}

/// Usage rows: one group per provider. Only a provider's first row
/// carries its id, continuation rows leave it blank so the id column
/// groups the windows visually (matching `aj`'s usage page).
///
/// Each row is up to three columns, tinted like [`auth_rows`]: the
/// right-aligned provider-id column in `styles.muted` (blank but still
/// `id_w` wide on continuation rows), the window/status label in the
/// default style, and the per-window status detail in `styles.muted`.
/// The label is padded to a shared width on rows that carry a detail, so
/// every detail value starts at the same column and lines up.
pub(crate) fn usage_rows(statuses: &[ProviderUsageStatus], styles: &ContentStyles) -> Vec<Row> {
    let now_ms = now_unix_ms();
    // Materialize each provider's rows first so we can size the id and
    // detail columns to the whole set before emitting spans.
    let groups: Vec<(&str, Vec<(String, Option<String>)>)> = statuses
        .iter()
        .map(|status| {
            (
                status.provider_id.as_str(),
                usage_status_rows(status, now_ms),
            )
        })
        .collect();
    let id_w = groups
        .iter()
        .map(|(id, _)| id.chars().count())
        .max()
        .unwrap_or(0);
    let label_w = groups
        .iter()
        .flat_map(|(_, group)| group.iter())
        .filter(|(_, detail)| detail.is_some())
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0);

    let mut rows = Vec::new();
    for (id, group) in &groups {
        for (i, (label, detail)) in group.iter().enumerate() {
            // The id shows only on a provider's first row. Continuation
            // rows keep the column width so the label column stays put.
            let prefix = if i == 0 { *id } else { "" };
            let mut row = vec![span(format!("{prefix:>id_w$}"), styles.muted)];
            match detail {
                Some(detail) => {
                    row.push(span(format!("  {label:<label_w$}"), Style::default()));
                    row.push(span(format!("  {detail}"), styles.muted));
                }
                None => row.push(span(format!("  {label}"), Style::default())),
            }
            rows.push(row);
        }
    }
    if rows.is_empty() {
        rows.push(plain("No usage sources."));
    }
    rows
}

/// One session-info row rendered from the shared `aj_app` digest.
/// Section headers sit at column 0 and key/value pairs indent to 2
/// columns, and a blank spacer is emitted as a single-space row so it
/// occupies a real line instead of collapsing to zero height in the
/// [`ListView`].
pub(crate) fn session_info_rows(stats: &SessionStats) -> Vec<Row> {
    let rows = aj_app::session_info::digest(stats);
    let key_width = rows
        .iter()
        .filter_map(|row| match row {
            aj_app::session_info::InfoRow::Kv { key, .. } => Some(key.chars().count()),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|row| match row {
            aj_app::session_info::InfoRow::Header(title) => plain(title.as_str()),
            aj_app::session_info::InfoRow::Kv { key, value } => {
                plain(format!("  {key:<key_width$}  {value}"))
            }
            // A single space, not the empty string: an empty `RichText`
            // row collapses to zero height in the `ListView`, which would
            // erase the gap between sections. One space forces a line.
            aj_app::session_info::InfoRow::Blank => plain(" "),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use aj_app::keybindings::ACTION_PALETTE_OPEN;
    use aj_app::theme::ColorMode;
    use aj_models::types::{Usage, UsageCost};
    use aj_session::SessionSettings;
    use vaxis::cell::Color;

    use super::*;

    /// Concatenate a row's span texts, so the plain-text `.contains(...)`
    /// assertions keep working on a styled row.
    fn row_text(row: &Row) -> String {
        row.iter().map(|s| s.text.as_str()).collect()
    }

    /// Join a set of rows into one plain-text blob for `.contains(...)`.
    fn rows_text(rows: &[Row]) -> String {
        rows.iter().map(row_text).collect::<Vec<_>>().join("\n")
    }

    /// Distinct muted/heading tints so a column left at the default fg
    /// fails the tinting assertions.
    fn test_styles() -> ContentStyles {
        ContentStyles {
            muted: Style {
                fg: Color::Index(2),
                ..Style::default()
            },
            heading: Style {
                fg: Color::Index(3),
                bold: true,
                ..Style::default()
            },
        }
    }

    /// The plain-text of the first row containing `needle`, so a test can
    /// pin the key and description that share one entry row.
    fn row_containing(rows: &[Row], needle: &str) -> String {
        rows.iter()
            .map(row_text)
            .find(|t| t.contains(needle))
            .unwrap_or_else(|| panic!("no row contains {needle:?}"))
    }

    #[test]
    fn help_rows_has_three_colored_section_headings() {
        let styles = test_styles();
        let rows = help_rows(&styles);
        for heading in [
            "Editor shortcuts",
            "Scrolling & navigation",
            "Command palette commands",
        ] {
            let row = rows
                .iter()
                .find(|r| row_text(r) == heading)
                .unwrap_or_else(|| panic!("missing heading {heading:?}"));
            // A heading is one span carrying the heading style, not the
            // default: dropping a section removes its heading and fails the
            // `find`, and leaving a heading default-styled fails here.
            assert_eq!(row.len(), 1, "heading is one span: {row:?}");
            assert_eq!(row[0].style, styles.heading, "heading {heading:?} tint");
            assert_ne!(row[0].style, Style::default());
        }

        // The injected `test_styles` above only proves the render applies
        // whatever heading style it is handed. The spec asks for a *colored*
        // heading out of the box, so the token `from_theme` picks must resolve
        // to a real foreground in the bundled palettes, not the terminal
        // default. Pointing `heading` back at an empty/uncolored token (which
        // renders bold-only) fails here for both themes.
        for (name, theme) in [
            ("dark", Theme::bundled_dark_with_mode(ColorMode::Truecolor)),
            (
                "light",
                Theme::bundled_light_with_mode(ColorMode::Truecolor),
            ),
        ] {
            let heading = ContentStyles::from_theme(&theme).heading;
            assert!(heading.bold, "{name}: heading must stay bold");
            assert_ne!(
                heading.fg,
                Color::Default,
                "{name}: heading must resolve to a real color, not the terminal default"
            );
        }
    }

    #[test]
    fn help_section_one_combines_editor_chords_and_resolved_globals() {
        let rows = help_rows(&test_styles());
        // Every editor chord flows straight from `TextArea::bindings()` as its
        // fixed `ChordDoc.keys` literal (these chords are not rebindable), so
        // each chord's keys and description share one row in section 1.
        // Iterating the whole table (like the section-3 command test iterates
        // `COMMANDS`) means dropping a whole chord group fails here: its
        // descriptions vanish and `row_containing` panics, rather than a lone
        // first-entry check staying green.
        for chord in TextArea::bindings() {
            assert!(
                row_containing(&rows, chord.description).contains(chord.keys),
                "editor chord {:?} must carry its keys {:?}",
                chord.description,
                chord.keys
            );
        }

        // A compose-time global flows from the keybinding data: the label on
        // the palette-open row equals `action_shortcut`, so both
        // sources feed section 1.
        let resolved =
            action_shortcut(ACTION_PALETTE_OPEN).expect("palette-open has a default chord");
        assert!(
            row_containing(&rows, "Open command palette").contains(&resolved),
            "palette-open row must carry the resolved label {resolved:?}"
        );
    }

    #[test]
    fn help_section_one_pins_spec_named_globals() {
        let rows = help_rows(&test_styles());
        // The spec names these compose-time globals for section 1. We pin each
        // by its action-id constant directly, not by iterating
        // `COMPOSE_GLOBAL_ACTIONS`, so dropping a const entry drops its row and
        // fails this named test (its description no longer appears, so
        // `row_containing` panics) rather than only tripping the unused-import
        // lint. Each expected label is resolved through `action_shortcut`,
        // never a literal, so a rebind updates the row and the expectation
        // together.
        for id in [
            ACTION_PALETTE_OPEN,
            ACTION_CLIPBOARD_PASTE_IMAGE,
            ACTION_THINKING_TOGGLE,
            ACTION_TOOLS_EXPAND,
        ] {
            let (key, desc) = global_chord(id).expect("spec-named global in the keybinding table");
            assert!(
                row_containing(&rows, desc).contains(&key),
                "section-1 global {desc:?} must carry the resolved label {key:?}"
            );
        }
    }

    #[test]
    fn help_section_two_lists_scroll_nav_with_resolved_labels() {
        let rows = help_rows(&test_styles());
        // Each scroll/nav action's row carries its resolved label next to its
        // description, so a hardcoded wrong label fails this assertion (the
        // expectation itself is data-derived).
        for id in SCROLL_NAV_ACTIONS {
            let (key, desc) = global_chord(id).expect("scroll/nav action in the table");
            assert!(
                row_containing(&rows, desc).contains(&key),
                "row for {desc:?} must carry the resolved label {key:?}"
            );
        }
        // The descriptive mouse-wheel row is present and marked as input.
        assert!(
            rows_text(&rows).contains("Mouse wheel"),
            "wheel row present"
        );
    }

    #[test]
    fn help_section_three_lists_every_command() {
        let rows = help_rows(&test_styles());
        let blob = rows_text(&rows);
        for cmd in COMMANDS {
            assert!(blob.contains(cmd.title), "missing command {}", cmd.title);
            // A command with a bound action carries its resolved shortcut.
            if let Some(short) = cmd.action_id.and_then(action_shortcut) {
                assert!(
                    row_containing(&rows, cmd.description).contains(&short),
                    "command {} must carry its resolved shortcut {short:?}",
                    cmd.title
                );
            }
        }
    }

    #[test]
    fn help_labels_are_resolved_not_hardcoded() {
        let rows = help_rows(&test_styles());
        // Every compose-time global label equals `action_shortcut`,
        // so the expectation is generated from the same data the row is. A
        // literal that drifts from the binding fails here.
        for id in COMPOSE_GLOBAL_ACTIONS {
            let (key, desc) = global_chord(id).expect("compose-time action in the table");
            assert!(
                row_containing(&rows, desc).contains(&key),
                "global {desc:?} must carry the resolved label {key:?}"
            );
        }
    }

    #[test]
    fn auth_rows_render_summary_and_detail() {
        let rows = rows_text(&auth_rows(
            &[
                ProviderAuthStatus {
                    provider_id: "anthropic".into(),
                    configured: true,
                    summary: "subscription".into(),
                    detail: Some("expires in 1h".into()),
                },
                ProviderAuthStatus {
                    provider_id: "openai".into(),
                    configured: false,
                    summary: "not configured".into(),
                    detail: None,
                },
            ],
            &test_styles(),
        ));
        assert!(rows.contains("anthropic"), "{rows}");
        assert!(rows.contains("subscription"), "{rows}");
        assert!(rows.contains("expires in 1h"), "{rows}");
        assert!(rows.contains("not configured"), "{rows}");
    }

    /// The auth page tints its columns: the provider id and detail in the
    /// muted style, the summary in the default style between them, and no
    /// parentheses around the detail. This fails if a column is
    /// left at the default fg.
    #[test]
    fn auth_rows_tint_columns_from_styles() {
        let styles = test_styles();
        let rows = auth_rows(
            &[ProviderAuthStatus {
                provider_id: "anthropic".into(),
                configured: true,
                summary: "subscription".into(),
                detail: Some("expires in 1h".into()),
            }],
            &styles,
        );
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.len(), 3, "id, summary, and detail spans: {row:?}");
        // Provider id in the muted tint.
        assert!(row[0].text.contains("anthropic"), "{row:?}");
        assert_eq!(row[0].style, styles.muted);
        // Summary in the default style.
        assert!(row[1].text.contains("subscription"), "{row:?}");
        assert_eq!(row[1].style, Style::default());
        // Detail in the muted tint, and no parentheses.
        assert!(row[2].text.contains("expires in 1h"), "{row:?}");
        assert_eq!(row[2].style, styles.muted);
        assert!(
            !row_text(row).contains('('),
            "detail carries no parentheses: {row:?}"
        );
    }

    /// A provider with no detail yields a two-span row (id + summary), so
    /// the empty case doesn't leave a stray muted span.
    #[test]
    fn auth_rows_without_detail_have_no_muted_span() {
        let styles = test_styles();
        let rows = auth_rows(
            &[ProviderAuthStatus {
                provider_id: "openai".into(),
                configured: false,
                summary: "not configured".into(),
                detail: None,
            }],
            &styles,
        );
        assert_eq!(rows[0].len(), 2, "{:?}", rows[0]);
    }

    /// The id column is right-aligned to the widest id, so a shorter id
    /// carries leading spaces and every id ends at the same column. The
    /// summary is padded on detail rows so the detail column starts at
    /// one shared position across rows.
    #[test]
    fn auth_rows_right_align_id_and_align_detail_column() {
        let styles = test_styles();
        let rows = auth_rows(
            &[
                ProviderAuthStatus {
                    provider_id: "anthropic".into(), // widest id (9)
                    configured: true,
                    summary: "subscription".into(),
                    detail: Some("expires in 1h".into()),
                },
                ProviderAuthStatus {
                    provider_id: "openai".into(), // shorter id (6)
                    configured: true,
                    summary: "api key".into(),
                    detail: Some("no expiry".into()),
                },
            ],
            &styles,
        );
        assert_eq!(rows.len(), 2);

        // Both id spans share the widest id's width, and the shorter id
        // is right-aligned so it carries leading spaces.
        let id0 = &rows[0][0];
        let id1 = &rows[1][0];
        assert_eq!(id0.text, "anthropic");
        assert_eq!(id1.text, "   openai");
        assert_eq!(id0.text.chars().count(), id1.text.chars().count());

        // The detail span begins at the same column on both rows: the
        // text up to (but not including) the detail is equal in width, so
        // the expiry values line up.
        let prefix_width = |row: &Row| -> usize {
            row[..row.len() - 1]
                .iter()
                .map(|s| s.text.chars().count())
                .sum()
        };
        assert_eq!(prefix_width(&rows[0]), prefix_width(&rows[1]));

        // Tints preserved: id muted, summary default, detail muted.
        assert_eq!(rows[0][0].style, styles.muted);
        assert_eq!(rows[0][1].style, Style::default());
        assert_eq!(rows[0][2].style, styles.muted);
    }

    #[test]
    fn usage_rows_group_windows_under_the_provider() {
        use aj_models::usage::{ProviderUsage, UsageWindow};
        let rows = rows_text(&usage_rows(
            &[
                ProviderUsageStatus {
                    provider_id: "anthropic".into(),
                    outcome: UsageOutcome::Usage(ProviderUsage {
                        windows: vec![UsageWindow {
                            label: "5-hour".into(),
                            used: 0.5,
                            resets_at: None,
                        }],
                        notes: Vec::new(),
                        reset_credits: None,
                    }),
                },
                ProviderUsageStatus {
                    provider_id: "openai".into(),
                    outcome: UsageOutcome::NotConfigured,
                },
            ],
            &test_styles(),
        ));
        assert!(rows.contains("anthropic"), "{rows}");
        assert!(rows.contains("5-hour"), "{rows}");
        assert!(rows.contains("50% used"), "{rows}");
        assert!(rows.contains("not configured"), "{rows}");
    }

    /// The usage page tints its columns like the auth page: the provider
    /// id and status detail in the muted style, the window label in the
    /// default style. A provider's continuation rows
    /// leave the id column blank so the group reads as one provider. This
    /// fails if a column is left at the default fg.
    #[test]
    fn usage_rows_tint_columns_from_styles() {
        use aj_models::usage::{ProviderUsage, UsageWindow};
        let styles = test_styles();
        let rows = usage_rows(
            &[ProviderUsageStatus {
                provider_id: "anthropic".into(),
                outcome: UsageOutcome::Usage(ProviderUsage {
                    // Labels of different widths (6 vs 5) so the
                    // detail-column alignment below genuinely exercises
                    // the label padding, not just equal-length labels.
                    windows: vec![
                        UsageWindow {
                            label: "5-hour".into(),
                            used: 0.5,
                            resets_at: None,
                        },
                        UsageWindow {
                            label: "7-day".into(),
                            used: 0.25,
                            resets_at: None,
                        },
                    ],
                    notes: Vec::new(),
                    reset_credits: None,
                }),
            }],
            &styles,
        );
        assert_eq!(rows.len(), 2, "one row per window: {rows:?}");

        // First row: id, label, and detail spans.
        let first = &rows[0];
        assert_eq!(first.len(), 3, "id, label, and detail spans: {first:?}");
        // Provider id in the muted tint, on the first row of the group.
        assert!(first[0].text.contains("anthropic"), "{first:?}");
        assert_eq!(first[0].style, styles.muted);
        // Window label in the default style.
        assert!(first[1].text.contains("5-hour"), "{first:?}");
        assert_eq!(first[1].style, Style::default());
        // Status detail in the muted tint.
        assert!(first[2].text.contains("50% used"), "{first:?}");
        assert_eq!(first[2].style, styles.muted);

        // Continuation row: id column is blank (no provider id), still in
        // the muted tint, and the label/detail carry the same tints.
        let second = &rows[1];
        assert_eq!(second.len(), 3, "id, label, and detail spans: {second:?}");
        assert!(
            second[0].text.trim().is_empty(),
            "continuation id column is blank: {second:?}"
        );
        assert_eq!(second[0].style, styles.muted);
        assert!(second[1].text.contains("7-day"), "{second:?}");
        assert_eq!(second[1].style, Style::default());
        assert_eq!(second[2].style, styles.muted);

        // The blank continuation id still occupies the id column width,
        // so the label column doesn't shift between the two rows.
        assert_eq!(
            first[0].text.chars().count(),
            second[0].text.chars().count(),
            "continuation id occupies id_w: {rows:?}"
        );

        // The detail column aligns even though the labels differ in
        // width: the text up to (but not including) the detail span is
        // equal width on both detail rows because the label is padded to
        // the shared width. This fails if the label weren't padded.
        let prefix_width = |row: &Row| -> usize {
            row[..row.len() - 1]
                .iter()
                .map(|s| s.text.chars().count())
                .sum()
        };
        assert_eq!(prefix_width(first), prefix_width(second));
    }

    /// The provider-id column is right-aligned to the widest id across
    /// providers, so a shorter id carries leading spaces and every id
    /// ends at the same column.
    #[test]
    fn usage_rows_right_align_id_across_providers() {
        use aj_models::usage::{ProviderUsage, UsageWindow};
        let rows = usage_rows(
            &[
                ProviderUsageStatus {
                    provider_id: "anthropic".into(), // widest id (9)
                    outcome: UsageOutcome::Usage(ProviderUsage {
                        windows: vec![UsageWindow {
                            label: "5-hour".into(),
                            used: 0.5,
                            resets_at: None,
                        }],
                        notes: Vec::new(),
                        reset_credits: None,
                    }),
                },
                ProviderUsageStatus {
                    provider_id: "openai".into(), // shorter id (6)
                    outcome: UsageOutcome::NotConfigured,
                },
            ],
            &test_styles(),
        );
        assert_eq!(rows[0][0].text, "anthropic");
        assert_eq!(rows[1][0].text, "   openai");
        assert_eq!(
            rows[0][0].text.chars().count(),
            rows[1][0].text.chars().count()
        );
    }

    fn sample_stats() -> SessionStats {
        SessionStats {
            session_id: "2026-06-19-14-22-03-512".to_string(),
            path: PathBuf::from("/home/u/.aj/sessions/home-u-proj/2026-06-19-14-22-03-512.jsonl"),
            created_at: None,
            last_activity: None,
            size_bytes: Some(48 * 1024),
            total_entries: 127,
            user_messages: 15,
            assistant_messages: 18,
            tool_results: 30,
            tool_calls: 31,
            tool_call_counts: vec![("read_file".to_string(), 12), ("Bash".to_string(), 8)],
            subagents: 2,
            compactions: 1,
            usage: Usage {
                input: 1_000,
                output: 2_000,
                cache_read: 500,
                cache_write: 250,
                total_tokens: 3_750,
                cost: UsageCost {
                    input: 0.10,
                    output: 0.20,
                    cache_read: 0.01,
                    cache_write: 0.02,
                    total: 0.33,
                },
            },
            settings: SessionSettings {
                model: Some(("anthropic".to_string(), "claude-sonnet-4-5".to_string())),
                thinking: Some("medium".to_string()),
                speed: None,
                verbosity: None,
            },
        }
    }

    #[test]
    fn session_info_rows_render_identity_counts_and_tools() {
        let rows = session_info_rows(&sample_stats());
        let blob = rows_text(&rows);
        assert!(blob.contains("2026-06-19-14-22-03-512"), "{blob}");
        assert!(blob.contains("home-u-proj"), "{blob}");
        assert!(blob.contains("anthropic / claude-sonnet-4-5"), "{blob}");
        assert!(blob.contains("48 KB"), "{blob}");
        assert!(blob.contains("read_file"), "{blob}");
        assert!(blob.contains("Tool calls (31)"), "{blob}");
        assert!(blob.contains("total tokens"), "{blob}");
        assert!(blob.contains("$0.3300"), "{blob}");

        // Section headers sit at column 0 and key/value rows indent to 2
        // columns.
        let texts: Vec<String> = rows.iter().map(row_text).collect();
        assert!(
            texts.iter().any(|t| t == "Session"),
            "header at column 0 (no leading space): {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.starts_with("  id ")),
            "key/value at column 2: {texts:?}"
        );

        // The blank spacer between sections occupies a real line: the row
        // before the "Settings" header is a single space, not the empty
        // string. An empty `RichText` collapses to zero height in the
        // `ListView`, so a single-space line is what keeps the gap visible.
        let settings = texts
            .iter()
            .position(|t| t == "Settings")
            .expect("Settings header present");
        assert!(settings >= 1, "Settings not first: {texts:?}");
        assert_eq!(
            texts[settings - 1],
            " ",
            "spacer row before Settings is a non-collapsing single-space \
             line, not an empty string: {texts:?}"
        );
    }

    /// A draw context bounded in both dimensions, so the overlay lays out
    /// against a real viewport and its scroll math has content to move
    /// through.
    fn draw_ctx(width: u16, height: u16) -> DrawContext {
        crate::test_support::draw_ctx(width, Some(height))
    }

    /// A content overlay over `n` single-line plain rows, taller than any
    /// viewport the scroll tests use.
    fn tall_overlay(n: usize) -> ContentOverlay {
        ContentOverlay::new((0..n).map(|i| plain(format!("line {i}"))).collect())
    }

    fn key_press(codepoint: u32, mods: Modifiers) -> Event {
        Event::KeyPress(Key {
            codepoint,
            mods,
            ..Key::default()
        })
    }

    /// The absolute line at the top of the viewport. With single-line rows
    /// the top item's index is its start line, so the absolute top is that
    /// index plus how far the item is scrolled above the viewport edge.
    fn absolute_top(overlay: &ContentOverlay) -> i32 {
        let list = overlay.list.borrow();
        i32::try_from(list.scroll_top()).expect("scroll_top fits i32") + list.scroll_offset()
    }

    /// A single Down advances the viewport by exactly one line. This is the
    /// regression the bug needed: the old cursor-item nav (`next_item`) moved
    /// nothing until the hidden cursor left the viewport, so the first
    /// viewport-worth of Down presses looked dead. Swapping `scroll_lines(1)`
    /// back for `next_item` leaves the top at 0 here and fails.
    #[test]
    fn single_down_scrolls_one_line() {
        let mut overlay = tall_overlay(50);
        let ctx = draw_ctx(40, 10);
        // First draw measures the viewport and pins the top at line 0.
        let _ = overlay.draw(&ctx);
        assert_eq!(absolute_top(&overlay), 0);

        let mut ec = EventContext::new();
        overlay.capture_event(&mut ec, &key_press(Key::DOWN, Modifiers::empty()));
        let _ = overlay.draw(&ctx);
        assert_eq!(
            absolute_top(&overlay),
            1,
            "one Down advances the viewport by a single line"
        );
    }

    /// PageDown scrolls by one viewport-scaled page, Home returns to the
    /// first line, and End lands at the bottom.
    #[test]
    fn page_down_home_and_end_move_the_viewport() {
        let mut overlay = tall_overlay(50);
        let ctx = draw_ctx(40, 10);
        let _ = overlay.draw(&ctx);
        // Read the page size the overlay computed against the drawn viewport,
        // so the expectation tracks whatever height the layout measured.
        let page = crate::scroll::page_scroll_lines(overlay.list.borrow().viewport_height());

        let mut ec = EventContext::new();
        overlay.capture_event(&mut ec, &key_press(Key::PAGE_DOWN, Modifiers::empty()));
        let _ = overlay.draw(&ctx);
        assert_eq!(
            absolute_top(&overlay),
            page,
            "PageDown scrolls one viewport-scaled page"
        );
        assert!(!overlay.list.borrow().is_at_bottom());

        let mut ec = EventContext::new();
        overlay.capture_event(&mut ec, &key_press(Key::HOME, Modifiers::empty()));
        let _ = overlay.draw(&ctx);
        assert_eq!(absolute_top(&overlay), 0, "Home returns to the top");

        let mut ec = EventContext::new();
        overlay.capture_event(&mut ec, &key_press(Key::END, Modifiers::empty()));
        let _ = overlay.draw(&ctx);
        assert!(
            overlay.list.borrow().is_at_bottom(),
            "End lands at the bottom"
        );
    }

    /// Ctrl+N and Ctrl+P scroll one line down and up, mirroring Down and Up.
    #[test]
    fn ctrl_n_and_ctrl_p_scroll_like_down_and_up() {
        let mut overlay = tall_overlay(50);
        let ctx = draw_ctx(40, 10);
        let _ = overlay.draw(&ctx);

        let mut ec = EventContext::new();
        overlay.capture_event(&mut ec, &key_press(u32::from('n'), Modifiers::CTRL));
        let _ = overlay.draw(&ctx);
        assert_eq!(absolute_top(&overlay), 1, "Ctrl+N scrolls down one line");

        let mut ec = EventContext::new();
        overlay.capture_event(&mut ec, &key_press(u32::from('p'), Modifiers::CTRL));
        let _ = overlay.draw(&ctx);
        assert_eq!(absolute_top(&overlay), 0, "Ctrl+P scrolls up one line");
    }

    /// The drawn thumb glyph carries the tint set via `set_thumb_style`,
    /// applied per-draw. This pins the draw-time apply: dropping the tint (a
    /// `Style::default()` no-op) fails here rather than passing on the shared
    /// default glyph, which already matches on its grapheme alone.
    #[test]
    fn scrollbar_thumb_carries_the_configured_tint() {
        let mut overlay = tall_overlay(50);
        // A distinct fg so a tinted thumb cell can't be confused with any
        // other column's default color.
        let tint = Color::Index(200);
        overlay.set_thumb_style(Style {
            fg: tint,
            ..Style::default()
        });
        let ctx = draw_ctx(20, 6);
        let surface = overlay.draw(&ctx);
        // The thumb sits on the list's right edge, in a child surface, so
        // composite the tree before reading the cell's style.
        let last_col = 19;
        let fg = crate::test_support::flatten(&surface)
            .iter()
            .find_map(|row| {
                let cell = row.get(last_col)?;
                (cell.char.grapheme() == "\u{2590}").then_some(cell.style.fg)
            })
            .expect("a thumb cell is drawn on the list's right edge");
        assert_eq!(
            fg, tint,
            "the content-overlay thumb carries the tint set via set_thumb_style"
        );
    }

    /// Driving `open_content_overlay` tints the drawn thumb with the chrome's
    /// Muted color, pinning the open-time wiring. Deleting the `set_thumb_style`
    /// call in `open_content_overlay` drops the tint and fails here.
    #[test]
    fn open_content_overlay_tints_the_thumb_from_the_chrome() {
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);
        let chrome = OverlayChrome::from_theme(&theme);
        let editor: WidgetRef = Rc::new(RefCell::new(RichText::new(plain(" "))));
        let stack = Rc::new(RefCell::new(OverlayStack::default()));
        let mut ctx = EventContext::new();
        let rows = (0..50).map(|i| plain(format!("line {i}"))).collect();
        open_content_overlay(&stack, &editor, &chrome, "Title", rows, &mut ctx);

        // Draw the pushed window and composite its child tree, then read the
        // thumb glyph's tint from wherever it lands inside the border.
        let draw = draw_ctx(24, 12);
        let stack_ref = stack.borrow();
        let window = &stack_ref.top().expect("open pushes an overlay").widget;
        let surface = window.borrow_mut().draw(&draw);
        let fg = crate::test_support::flatten(&surface)
            .iter()
            .flatten()
            .find_map(|cell| (cell.char.grapheme() == "\u{2590}").then_some(cell.style.fg))
            .expect("a thumb cell is drawn inside the window");
        assert_eq!(
            fg, chrome.select.scrollbar_thumb.fg,
            "the open path tints the thumb with the chrome's Muted color"
        );
    }

    /// A plain builder produces single-span, default-styled rows, pinning
    /// the appearance-preserving path: a plain row is one default span, so
    /// it draws exactly as a plain-string row would.
    #[test]
    fn plain_builder_rows_are_single_default_spans() {
        for row in session_info_rows(&sample_stats()) {
            assert_eq!(row.len(), 1, "plain row is one span: {row:?}");
            assert_eq!(
                row[0].style,
                Style::default(),
                "plain row default-styled: {row:?}"
            );
        }
    }
}
