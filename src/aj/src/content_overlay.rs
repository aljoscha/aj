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
use aj_models::auth::{AccountLabelDisplayMode, display_account_label};
use aj_session::SessionStats;
use unicode_segmentation::UnicodeSegmentation;
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

const ACCOUNT_ROW_CELL_LIMIT: u16 = u16::MAX;
const ACCOUNT_ROW_CLIPPED_PREFIX_CELLS: usize = 96;
const ACCOUNT_ROW_CLIPPED_NOTICE: &str =
    "[clipped; complete account row exceeds 65,535-cell terminal limit] ";

/// Measure terminal cells without accumulating the complete string in `u16`.
/// Account representations above the vaxis extent are graphic ASCII, so the
/// fast path also avoids asking `gwidth` to represent an impossible total.
fn terminal_cells(text: &str, method: vaxis::gwidth::Method) -> usize {
    if text.is_ascii() {
        return text.len();
    }
    text.graphemes(true)
        .map(|grapheme| usize::from(vaxis::gwidth::gwidth(grapheme, method)))
        .sum()
}

/// Keep a disclosed prefix of an account representation within its share of
/// the complete row. The raw account remains in storage and action models;
/// this bounds only the read-only RichText surface.
fn account_label_for_row(
    represented: &str,
    cell_budget: usize,
    method: vaxis::gwidth::Method,
) -> String {
    if terminal_cells(represented, method) <= cell_budget {
        return represented.to_string();
    }

    let notice_cells = terminal_cells(ACCOUNT_ROW_CLIPPED_NOTICE, method);
    if notice_cells > cell_budget {
        return "[clipped]".chars().take(cell_budget).collect();
    }

    let prefix_budget = cell_budget
        .saturating_sub(notice_cells)
        .min(ACCOUNT_ROW_CLIPPED_PREFIX_CELLS);
    let mut prefix = String::new();
    let mut prefix_cells = 0;
    for grapheme in represented.graphemes(true) {
        let width = terminal_cells(grapheme, method);
        if prefix_cells + width > prefix_budget {
            break;
        }
        prefix.push_str(grapheme);
        prefix_cells += width;
    }
    format!("{ACCOUNT_ROW_CLIPPED_NOTICE}{prefix}")
}

/// Auth-status rows: one per provider/account credential, its default marker,
/// credential summary, and any secondary detail (e.g. token expiry). Providers
/// without labeled accounts contribute one provider-only row.
///
/// Labeled rows add account and default-marker columns between the provider id
/// and summary. The provider id and secondary detail use `styles.muted`. The
/// account and summary use the default style. The id column is right-aligned,
/// account padding follows terminal cell width, and the summary is padded to a
/// shared width on rows that carry a detail so subsequent columns line up.
pub(crate) fn auth_rows(
    statuses: &[ProviderAuthStatus],
    styles: &ContentStyles,
    width_method: vaxis::gwidth::Method,
) -> Vec<Row> {
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
    let ids = statuses
        .iter()
        .map(|status| format!("{id:>id_w$}", id = status.provider_id))
        .collect::<Vec<_>>();
    let summaries = statuses
        .iter()
        .map(|status| {
            if status.detail.is_some() {
                format!("  {summary:<summary_w$}", summary = status.summary)
            } else {
                format!("  {summary}", summary = status.summary)
            }
        })
        .collect::<Vec<_>>();

    // ListView measures each soft-wrapped RichText child without a height cap.
    // At a one-cell body width, every cell becomes one row, so budget the shared
    // account column against the widest complete fixed row before RichText sees
    // it. This keeps the row count representable in u16 at every terminal width.
    let account_cell_budget = statuses
        .iter()
        .enumerate()
        .filter(|(_, status)| status.account_label.is_some())
        .map(|(index, status)| {
            let detail_cells = status
                .detail
                .as_ref()
                .map(|detail| terminal_cells(&format!("  {detail}"), width_method))
                .unwrap_or(0);
            let fixed_cells = terminal_cells(&ids[index], width_method)
                + 2 // account-column separator
                + 9 // default marker or matching padding
                + terminal_cells(&summaries[index], width_method)
                + detail_cells;
            usize::from(ACCOUNT_ROW_CELL_LIMIT).saturating_sub(fixed_cells)
        })
        .min()
        .unwrap_or_else(|| usize::from(ACCOUNT_ROW_CELL_LIMIT));
    let represented = statuses
        .iter()
        .map(|status| {
            status.account_label.as_deref().map(|label| {
                let ordinary = display_account_label(label, AccountLabelDisplayMode::Ordinary);
                let represented = if ordinary.contains(' ') {
                    display_account_label(label, AccountLabelDisplayMode::Ascii)
                } else {
                    ordinary
                };
                account_label_for_row(&represented, account_cell_budget, width_method)
            })
        })
        .collect::<Vec<_>>();
    let account_w = represented
        .iter()
        .filter_map(|label| label.as_ref())
        .map(|label| terminal_cells(label, width_method))
        .max()
        .unwrap_or(0);
    statuses
        .iter()
        .enumerate()
        .map(|(index, s)| {
            let mut row = vec![span(ids[index].clone(), styles.muted)];
            if let Some(label) = &represented[index] {
                let label_w = terminal_cells(label, width_method);
                let padding = " ".repeat(account_w.saturating_sub(label_w));
                row.push(span(format!("  {label}{padding}"), Style::default()));
                row.push(span(
                    if s.is_default {
                        "  default"
                    } else {
                        "         "
                    },
                    styles.muted,
                ));
            }
            row.push(span(summaries[index].clone(), Style::default()));
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
            if usage.windows.is_empty() && usage.notes.is_empty() && usage.reset_offer.is_none() {
                out.push(("no usage data reported".to_string(), None));
            }
            for window in &usage.windows {
                let desc = format_window_status(window.used, window.resets_at, now_ms);
                out.push((window.label.clone(), Some(desc)));
            }
            for note in &usage.notes {
                out.push((note.clone(), None));
            }
            if let Some(offer) = &usage.reset_offer {
                let available = offer.available();
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

/// Usage rows: one group per provider account. Only the group's first row
/// carries its provider and optional account identity; continuation rows
/// leave those columns blank so the windows stay grouped visually. When no
/// labeled account exists, the account column is omitted and the pre-account
/// three-column layout stays exact.
///
/// Each row is up to four columns, tinted like [`auth_rows`]: the
/// right-aligned provider-id column in `styles.muted`, an optional account
/// column in the same tint, the window/status label in the default style,
/// and the per-window status detail in `styles.muted`.
/// The label is padded to a shared width on rows that carry a detail, so
/// every detail value starts at the same column and lines up.
pub(crate) fn usage_rows(
    statuses: &[ProviderUsageStatus],
    styles: &ContentStyles,
    width_method: vaxis::gwidth::Method,
) -> Vec<Row> {
    let now_ms = now_unix_ms();
    // Materialize each account's rows first so every identity uses the shared
    // reversible representation and every column can be sized before spans
    // are emitted.
    let mut groups: Vec<(&str, Option<String>, Vec<(String, Option<String>)>)> = statuses
        .iter()
        .map(|status| {
            let represented = status.account.as_deref().map(|account| {
                let ordinary = display_account_label(account, AccountLabelDisplayMode::Ordinary);
                if ordinary.contains(' ') {
                    display_account_label(account, AccountLabelDisplayMode::Ascii)
                } else {
                    ordinary
                }
            });
            (
                status.provider_id.as_str(),
                represented,
                usage_status_rows(status, now_ms),
            )
        })
        .collect();
    let has_accounts = groups.iter().any(|(_, account, _)| account.is_some());
    let id_w = groups
        .iter()
        .map(|(id, _, _)| id.chars().count())
        .max()
        .unwrap_or(0);
    let label_w = groups
        .iter()
        .flat_map(|(_, _, group)| group.iter())
        .filter(|(_, detail)| detail.is_some())
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0);

    // At one terminal cell wide, soft wrapping turns every cell into a row.
    // Bound the account's share against the complete first row before handing
    // it to RichText, including the longest status tail it sits beside.
    let account_cell_budget = groups
        .iter()
        .filter_map(|(_, account, group)| {
            account.as_ref()?;
            let (label, detail) = group.first()?;
            let tail = match detail {
                Some(detail) => format!("  {label:<label_w$}  {detail}"),
                None => format!("  {label}"),
            };
            let fixed_cells = id_w + 2 + terminal_cells(&tail, width_method);
            Some(usize::from(ACCOUNT_ROW_CELL_LIMIT).saturating_sub(fixed_cells))
        })
        .min()
        .unwrap_or_else(|| usize::from(ACCOUNT_ROW_CELL_LIMIT));
    for (_, account, _) in &mut groups {
        if let Some(represented) = account {
            *represented = account_label_for_row(represented, account_cell_budget, width_method);
        }
    }
    let account_w = groups
        .iter()
        .filter_map(|(_, account, _)| account.as_deref())
        .map(|account| terminal_cells(account, width_method))
        .max()
        .unwrap_or(0);

    let mut rows = Vec::new();
    for (id, account, group) in &groups {
        for (i, (label, detail)) in group.iter().enumerate() {
            // Identity shows only on an account's first row. Continuation
            // rows keep both widths so the window label does not shift.
            let prefix = if i == 0 { *id } else { "" };
            let mut row = vec![span(format!("{prefix:>id_w$}"), styles.muted)];
            if has_accounts {
                let account = if i == 0 {
                    account.as_deref().unwrap_or("")
                } else {
                    ""
                };
                let padding =
                    " ".repeat(account_w.saturating_sub(terminal_cells(account, width_method)));
                row.push(span(format!("  {account}{padding}"), styles.muted));
            }
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
///
/// Ordinary digest fields are folded to one line at this render boundary.
/// Environment pairs retain their typed row until here, where both sides are
/// quoted and escaped to an ASCII-only representation. Long representations
/// are split into numbered continuation rows before they reach [`RichText`].
/// This keeps every valid pair distinguishable and terminal-inert while
/// bounding the work and height of each unbounded [`ListView`] child.
pub(crate) fn session_info_rows(stats: &SessionStats, tag: Option<&str>) -> Vec<Row> {
    let tag = tag.map(crate::text::one_line);
    let rows = aj_app::session_info::digest(stats, tag.as_deref());
    let key_width = rows
        .iter()
        .filter_map(|row| match row {
            aj_app::session_info::InfoRow::Kv { key, .. } => {
                Some(crate::text::one_line(key).chars().count())
            }
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let mut rendered = Vec::new();
    for row in rows {
        match row {
            aj_app::session_info::InfoRow::Header(title) => rendered.push(plain(title)),
            aj_app::session_info::InfoRow::Kv { key, value } => {
                let key = crate::text::one_line(&key);
                let value = crate::text::one_line(&value);
                rendered.push(plain(format!("  {key:<key_width$}  {value}")));
            }
            aj_app::session_info::InfoRow::Env { key, value } => {
                rendered.extend(environment_rows(&key, &value));
            }
            // A single space, not the empty string: an empty `RichText`
            // row collapses to zero height in the `ListView`, which would
            // erase the gap between sections. One space forces a line.
            aj_app::session_info::InfoRow::Blank => rendered.push(plain(" ")),
        }
    }
    rendered
}

/// Maximum escaped pair payload carried by one environment continuation row.
///
/// `ListView` measures each child without a height bound. `RichText` currently
/// scans the remaining word for each narrow soft-wrapped line, so bounding the
/// child makes measurement linear in the complete pair size and keeps every
/// child's height far below `u16::MAX`, even at a one-cell content width.
const ENV_ROW_PAYLOAD_CELLS: usize = 256;

/// Render one lossless escaped pair as one row, or numbered continuation rows.
/// Concatenating the payload after each `[part/total] ` prefix recovers exactly
/// the same `"key"="value"` representation as the single-row form.
fn environment_rows(key: &str, value: &str) -> Vec<Row> {
    let representation = format!("{}={}", quoted_env_text(key), quoted_env_text(value));
    debug_assert!(representation.is_ascii());
    let total = representation.len().div_ceil(ENV_ROW_PAYLOAD_CELLS);
    if total == 1 {
        return vec![plain(format!("  {representation}"))];
    }

    representation
        .as_bytes()
        .chunks(ENV_ROW_PAYLOAD_CELLS)
        .enumerate()
        .map(|(part, chunk)| {
            let payload = std::str::from_utf8(chunk).expect("escaped environment text is ASCII");
            plain(format!("  [{}/{total}] {payload}", part + 1))
        })
        .collect()
}

/// Quote arbitrary persisted environment text with an injective representation
/// made only of graphic ASCII. Spaces are explicit too: [`RichText`] may drop
/// separator whitespace at a soft-wrap boundary, while `\x20` remains visible
/// and reconstructable wherever the terminal wraps the row.
fn quoted_env_text(value: &str) -> String {
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        if ch == ' ' {
            quoted.push_str("\\x20");
        } else {
            quoted.extend(ch.escape_default());
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use aj_app::keybindings::ACTION_PALETTE_OPEN;
    use aj_app::theme::ColorMode;
    use aj_models::types::{Usage, UsageCost};
    use aj_session::{SessionSettings, UsageBucket};
    use vaxis::cell::Color;
    use vaxis::gwidth::Method;

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

    /// Recover one escaped environment pair from its single row or numbered
    /// continuation rows. `start` is a prefix of the escaped pair payload.
    fn continued_env_text(rows: &[String], start: &str) -> String {
        fn continuation(row: &str) -> Option<(usize, usize, &str)> {
            let rest = row.strip_prefix("  [")?;
            let (label, payload) = rest.split_once("] ")?;
            let (part, total) = label.split_once('/')?;
            Some((part.parse().ok()?, total.parse().ok()?, payload))
        }
        let at = rows
            .iter()
            .position(|row| {
                row.strip_prefix("  ")
                    .is_some_and(|payload| payload.starts_with(start))
                    || continuation(row).is_some_and(|(_, _, payload)| payload.starts_with(start))
            })
            .unwrap_or_else(|| panic!("no environment row starts with {start:?}"));
        let Some((part, total, first)) = continuation(&rows[at]) else {
            return rows[at]
                .strip_prefix("  ")
                .expect("single environment row is indented")
                .to_string();
        };
        assert_eq!(part, 1, "the first matching continuation starts the pair");
        let mut joined = String::from(first);
        for (offset, row) in rows[at + 1..at + total].iter().enumerate() {
            let (part, found_total, payload) =
                continuation(row).expect("the pair continues on numbered rows");
            assert_eq!(part, offset + 2, "continuation order");
            assert_eq!(found_total, total, "continuation total");
            joined.push_str(payload);
        }
        joined
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
                    account_label: None,
                    is_default: false,
                    configured: true,
                    summary: "subscription".into(),
                    detail: Some("expires in 1h".into()),
                },
                ProviderAuthStatus {
                    provider_id: "openai".into(),
                    account_label: None,
                    is_default: false,
                    configured: false,
                    summary: "not configured".into(),
                    detail: None,
                },
            ],
            &test_styles(),
            Method::Unicode,
        ));
        assert!(rows.contains("anthropic"), "{rows}");
        assert!(rows.contains("subscription"), "{rows}");
        assert!(rows.contains("expires in 1h"), "{rows}");
        assert!(rows.contains("not configured"), "{rows}");
    }

    #[test]
    fn auth_rows_represent_each_account_injectively_and_mark_only_the_default() {
        let rows = rows_text(&auth_rows(
            &[
                ProviderAuthStatus {
                    provider_id: "anthropic".into(),
                    account_label: Some("work".into()),
                    is_default: true,
                    configured: true,
                    summary: "subscription".into(),
                    detail: None,
                },
                ProviderAuthStatus {
                    provider_id: "anthropic".into(),
                    account_label: Some("wo\nrk".into()),
                    is_default: false,
                    configured: true,
                    summary: "API key (stored)".into(),
                    detail: None,
                },
            ],
            &test_styles(),
            Method::Unicode,
        ));
        assert!(rows.contains("work"), "{rows}");
        assert!(
            rows.contains("\\!\\u{77}\\u{6f}\\u{a}\\u{72}\\u{6b}"),
            "{rows}"
        );
        assert_eq!(rows.matches("default").count(), 1, "{rows}");
        assert!(
            !rows.contains("wo\nrk"),
            "raw control-bearing label leaked: {rows:?}"
        );
    }

    #[test]
    fn auth_rows_keep_complete_distinct_labels_below_the_inspection_limit() {
        let left = format!("{}x", "a".repeat(96));
        let right = format!("{}y", "a".repeat(96));
        let rows = rows_text(&auth_rows(
            &[
                ProviderAuthStatus {
                    provider_id: "provider".into(),
                    account_label: Some(left.clone()),
                    is_default: true,
                    configured: true,
                    summary: "subscription".into(),
                    detail: None,
                },
                ProviderAuthStatus {
                    provider_id: "provider".into(),
                    account_label: Some(right.clone()),
                    is_default: false,
                    configured: true,
                    summary: "subscription".into(),
                    detail: None,
                },
            ],
            &test_styles(),
            Method::Unicode,
        ));
        assert!(rows.contains(&left), "left tail missing: {rows}");
        assert!(rows.contains(&right), "right tail missing: {rows}");
        assert!(
            !rows.contains("[clipped"),
            "sub-limit labels were rewritten: {rows}"
        );
    }

    #[test]
    fn auth_rows_encode_spaces_before_the_prose_wrapper() {
        let rows = rows_text(&auth_rows(
            &[
                ProviderAuthStatus {
                    provider_id: "provider".into(),
                    account_label: Some("a b".into()),
                    is_default: true,
                    configured: true,
                    summary: "subscription".into(),
                    detail: None,
                },
                ProviderAuthStatus {
                    provider_id: "provider".into(),
                    account_label: Some("a    b".into()),
                    is_default: false,
                    configured: true,
                    summary: "subscription".into(),
                    detail: None,
                },
            ],
            &test_styles(),
            Method::Unicode,
        ));
        assert!(rows.contains("\\!\\u{61}\\u{20}\\u{62}"), "{rows}");
        assert!(rows.contains("\\u{20}\\u{20}\\u{20}\\u{20}"), "{rows}");
        assert!(
            !rows.contains("a    b"),
            "raw trimmable identity reached RichText"
        );
    }

    #[test]
    fn auth_rows_disclose_bounded_geometry_for_over_limit_legacy_labels() {
        let label = format!("{}\u{1000}", "a".repeat(10_921));
        let rows = rows_text(&auth_rows(
            &[ProviderAuthStatus {
                provider_id: "provider".into(),
                account_label: Some(label),
                is_default: true,
                configured: true,
                summary: "API key (stored)".into(),
                detail: None,
            }],
            &test_styles(),
            Method::Unicode,
        ));
        assert!(
            rows.contains("[clipped; complete auth row exceeds 65,535-cell terminal limit]"),
            "{rows}"
        );
        assert!(
            !rows.contains("\\u{1000}"),
            "the final tail entered vaxis: {rows}"
        );
        assert!(
            rows.len() < 512,
            "bounded row grew unexpectedly: {}",
            rows.len()
        );
    }

    #[test]
    fn auth_rows_budget_the_exact_limit_label_against_the_complete_row() {
        let label = format!("{}\u{0100}", "a".repeat(10_921));
        let rows = auth_rows(
            &[ProviderAuthStatus {
                provider_id: "provider".into(),
                account_label: Some(label),
                is_default: true,
                configured: true,
                summary: "API key (stored)".into(),
                detail: Some("legacy credential".into()),
            }],
            &test_styles(),
            Method::Unicode,
        );
        let text = rows_text(&rows);
        assert!(
            text.contains("[clipped; complete auth row exceeds 65,535-cell terminal limit]"),
            "{text}"
        );
        assert!(
            !text.contains("\\u{100}"),
            "the exact-limit tail entered RichText: {text}"
        );
        let cells = rows[0]
            .iter()
            .map(|segment| terminal_cells(&segment.text, Method::Unicode))
            .sum::<usize>();
        assert!(
            cells <= usize::from(ACCOUNT_ROW_CELL_LIMIT),
            "complete row has {cells} cells"
        );
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
                account_label: None,
                is_default: false,
                configured: true,
                summary: "subscription".into(),
                detail: Some("expires in 1h".into()),
            }],
            &styles,
            Method::Unicode,
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
                account_label: None,
                is_default: false,
                configured: false,
                summary: "not configured".into(),
                detail: None,
            }],
            &styles,
            Method::Unicode,
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
                    account_label: None,
                    is_default: false,
                    configured: true,
                    summary: "subscription".into(),
                    detail: Some("expires in 1h".into()),
                },
                ProviderAuthStatus {
                    provider_id: "openai".into(), // shorter id (6)
                    account_label: None,
                    is_default: false,
                    configured: true,
                    summary: "api key".into(),
                    detail: Some("no expiry".into()),
                },
            ],
            &styles,
            Method::Unicode,
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
    fn auth_rows_align_account_columns_with_the_renderer_width_method() {
        let assert_aligned = |left: &str, right: &str, method| {
            let rows = auth_rows(
                &[
                    ProviderAuthStatus {
                        provider_id: "provider".into(),
                        account_label: Some(left.into()),
                        is_default: false,
                        configured: true,
                        summary: "credential".into(),
                        detail: None,
                    },
                    ProviderAuthStatus {
                        provider_id: "provider".into(),
                        account_label: Some(right.into()),
                        is_default: false,
                        configured: true,
                        summary: "credential".into(),
                        detail: None,
                    },
                ],
                &test_styles(),
                method,
            );
            let mut ctx = crate::test_support::draw_ctx(80, Some(1));
            ctx.width_method = method;
            let summary_col = |row: &Row| {
                let widget = row_widgets(std::slice::from_ref(row))
                    .pop()
                    .expect("one rendered auth row");
                let surface = vaxis::vxfw::draw_widget(&widget, &ctx);
                crate::test_support::flatten(&surface)[0]
                    .iter()
                    .position(|cell| cell.char.grapheme() == "c")
                    .expect("credential summary in rendered row")
            };

            assert_eq!(
                summary_col(&rows[0]),
                summary_col(&rows[1]),
                "summary columns for {left:?} and {right:?} under {method:?}"
            );
        };

        assert_eq!(vaxis::gwidth::gwidth("個人", Method::Unicode), 4);
        assert_eq!(vaxis::gwidth::gwidth("work", Method::Unicode), 4);
        assert_aligned("個人", "work", Method::Unicode);

        assert_eq!(vaxis::gwidth::gwidth("👋🏿", Method::Wcwidth), 4);
        assert_eq!(vaxis::gwidth::gwidth("個", Method::Wcwidth), 2);
        assert_aligned("👋🏿", "個", Method::Wcwidth);
    }

    #[test]
    fn usage_rows_group_windows_under_the_provider() {
        use aj_models::usage::{ProviderUsage, UsageWindow};
        let rows = rows_text(&usage_rows(
            &[
                ProviderUsageStatus {
                    provider_id: "anthropic".into(),
                    account: None,
                    outcome: UsageOutcome::Usage(ProviderUsage {
                        windows: vec![UsageWindow {
                            label: "5-hour".into(),
                            used: 0.5,
                            resets_at: None,
                        }],
                        notes: Vec::new(),
                        reset_offer: None,
                    }),
                },
                ProviderUsageStatus {
                    provider_id: "openai".into(),
                    account: None,
                    outcome: UsageOutcome::NotConfigured,
                },
            ],
            &test_styles(),
            Method::Unicode,
        ));
        assert!(rows.contains("anthropic"), "{rows}");
        assert!(rows.contains("5-hour"), "{rows}");
        assert!(rows.contains("50% used"), "{rows}");
        assert!(rows.contains("not configured"), "{rows}");
    }

    #[test]
    fn usage_rows_render_one_sanitized_group_per_account() {
        use aj_models::usage::{ProviderUsage, UsageWindow};

        let styles = test_styles();
        let rows = usage_rows(
            &[
                ProviderUsageStatus {
                    provider_id: "anthropic".into(),
                    account: Some("personal".into()),
                    outcome: UsageOutcome::Usage(ProviderUsage {
                        windows: vec![
                            UsageWindow {
                                label: "5-hour".into(),
                                used: 0.5,
                                resets_at: None,
                            },
                            UsageWindow {
                                label: "weekly".into(),
                                used: 0.25,
                                resets_at: None,
                            },
                        ],
                        notes: vec!["Personal usage credits".into()],
                        reset_offer: Some(aj_models::usage::ResetCreditOffer::new(
                            1,
                            aj_models::usage::ResetCreditTarget::new(
                                "anthropic",
                                Some("personal".into()),
                                "personal-upstream",
                            ),
                        )),
                    }),
                },
                ProviderUsageStatus {
                    provider_id: "anthropic".into(),
                    account: Some("wo\nrk".into()),
                    outcome: UsageOutcome::Error("limit fetch failed".into()),
                },
            ],
            &styles,
            Method::Unicode,
        );

        assert_eq!(
            rows.len(),
            5,
            "two windows, one note, one reset-credit row, and one work error"
        );
        assert_eq!(rows[0][0].text.trim(), "anthropic");
        assert_eq!(
            rows[0][1].text.trim(),
            "personal",
            "the account label has its own rendered identity column"
        );
        assert_eq!(rows[0][0].style, styles.muted);
        assert_eq!(rows[0][1].style, styles.muted);
        assert!(rows[0][2].text.contains("5-hour"), "{:?}", rows[0]);
        assert_eq!(rows[0][2].style, Style::default());
        assert!(rows[0][3].text.contains("50% used"), "{:?}", rows[0]);
        assert_eq!(rows[0][3].style, styles.muted);
        assert!(rows[1][0].text.trim().is_empty(), "{:?}", rows[1]);
        assert!(rows[1][1].text.trim().is_empty(), "{:?}", rows[1]);
        assert!(
            row_text(&rows[2]).contains("Personal usage credits"),
            "{:?}",
            rows[2]
        );
        assert!(
            row_text(&rows[3]).contains("Rate-limit resets")
                && row_text(&rows[3]).contains("1 available"),
            "{:?}",
            rows[3]
        );
        for continuation in &rows[1..4] {
            assert!(continuation[0].text.trim().is_empty(), "{continuation:?}");
            assert!(continuation[1].text.trim().is_empty(), "{continuation:?}");
        }
        assert_eq!(rows[4][0].text.trim(), "anthropic");
        assert_eq!(rows[4][1].text.trim(), r"\!\u{77}\u{6f}\u{a}\u{72}\u{6b}");
        assert!(
            row_text(&rows[4]).contains("error: limit fetch failed"),
            "{:?}",
            rows[4]
        );
        assert!(
            rows.iter().all(|row| !row_text(row).contains(['\n', '\r'])),
            "free-text labels stay on one drawable line: {rows:?}"
        );
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
                account: None,
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
                    reset_offer: None,
                }),
            }],
            &styles,
            Method::Unicode,
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
                    account: None,
                    outcome: UsageOutcome::Usage(ProviderUsage {
                        windows: vec![UsageWindow {
                            label: "5-hour".into(),
                            used: 0.5,
                            resets_at: None,
                        }],
                        notes: Vec::new(),
                        reset_offer: None,
                    }),
                },
                ProviderUsageStatus {
                    provider_id: "openai".into(), // shorter id (6)
                    account: None,
                    outcome: UsageOutcome::NotConfigured,
                },
            ],
            &test_styles(),
            Method::Unicode,
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
                incomplete: false,
            },
            usage_breakdown: vec![UsageBucket {
                provider: "anthropic".to_string(),
                model: "claude-sonnet-4-5".to_string(),
                account: None,
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
                    incomplete: false,
                },
                responses: 18,
                unpriced_responses: 0,
            }],
            compaction_usage: Usage::default(),
            compactions_with_usage: 0,
            settings: SessionSettings {
                model: Some(("anthropic".to_string(), "claude-sonnet-4-5".to_string())),
                thinking: Some("medium".to_string()),
                speed: None,
                verbosity: None,
            },
            session_env: None,
        }
    }

    /// The tag supplements the id rather than standing in for it: both rows
    /// are on the page, adjacent, and the id row still carries the id. An
    /// untagged session keeps the same shape and says so.
    #[test]
    fn the_tag_row_sits_beside_the_id_row_without_replacing_it() {
        let rows = session_info_rows(&sample_stats(), Some("fix-auth"));
        let texts: Vec<String> = rows.iter().map(row_text).collect();
        let id_at = texts
            .iter()
            .position(|t| t.starts_with("  id ") && t.contains("2026-06-19-14-22-03-512"))
            .unwrap_or_else(|| panic!("an id row: {texts:?}"));
        let tag_at = texts
            .iter()
            .position(|t| t.starts_with("  tag ") && t.contains("fix-auth"))
            .unwrap_or_else(|| panic!("a tag row: {texts:?}"));
        assert_eq!(tag_at, id_at + 1, "the tag row follows the id: {texts:?}");

        let untagged: Vec<String> = session_info_rows(&sample_stats(), None)
            .iter()
            .map(row_text)
            .collect();
        assert_eq!(untagged.len(), texts.len(), "same shape when untagged");
        assert!(
            untagged[tag_at].starts_with("  tag ") && untagged[tag_at].contains("(none)"),
            "the row stays and says so: {:?}",
            untagged[tag_at],
        );
    }

    #[test]
    fn session_info_rows_render_identity_counts_and_tools() {
        let rows = session_info_rows(&sample_stats(), Some("fix-auth"));
        let blob = rows_text(&rows);
        assert!(blob.contains("2026-06-19-14-22-03-512"), "{blob}");
        assert!(blob.contains("home-u-proj"), "{blob}");
        assert!(blob.contains("anthropic / claude-sonnet-4-5"), "{blob}");
        assert!(blob.contains("48 KB"), "{blob}");
        assert!(blob.contains("read_file"), "{blob}");
        assert!(blob.contains("Tool calls (31)"), "{blob}");
        assert!(blob.contains("total tokens"), "{blob}");
        assert!(blob.contains("$0.3300"), "{blob}");
        let breakdown = row_containing(&rows, "3750 tokens · $0.3300");
        assert!(
            breakdown.contains("anthropic / claude-sonnet-4-5"),
            "the provider/model breakdown reached one rendered row: {breakdown}",
        );

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

    /// Environment rows are lossless, terminal-inert, and independently
    /// bounded. Pathological valid text draws in a narrow viewport while a long
    /// key cannot push the neighboring identity value behind unbounded padding.
    #[test]
    fn session_info_rows_render_only_the_recorded_environment() {
        let without_env = session_info_rows(&sample_stats(), None);
        assert!(
            !without_env.iter().any(|row| row_text(row) == "Env"),
            "an absent environment rendered a section: {:?}",
            without_env.iter().map(row_text).collect::<Vec<_>>(),
        );

        let mut stats = sample_stats();
        let long_zwj_grapheme = std::iter::repeat_n("👩", 130)
            .collect::<Vec<_>>()
            .join("\u{200d}");
        let long_key = "Z".repeat(2_048);
        stats.session_env = Some(std::collections::BTreeMap::from([
            ("BEADS_ACTOR".to_string(), "azurite ".to_string()),
            ("COMBINED_e\u{301}".to_string(), "plain".to_string()),
            ("DISTINCT_NEWLINE".to_string(), "A\nB".to_string()),
            ("DISTINCT_PLAIN".to_string(), "AB".to_string()),
            ("DISTINCT_TAB".to_string(), "a\tb".to_string()),
            ("DISTINCT_UNTABBED".to_string(), "ab".to_string()),
            (
                "FORMAT\u{202e}".to_string(),
                "line\u{2028}separator".to_string(),
            ),
            ("WIDE_界".to_string(), "wide".to_string()),
            ("ZWJ".to_string(), long_zwj_grapheme),
            (long_key.clone(), "long-key-value".to_string()),
        ]));
        let rows = session_info_rows(&stats, None);
        let texts: Vec<String> = rows.iter().map(row_text).collect();
        let settings = texts
            .iter()
            .position(|row| row == "Settings")
            .expect("Settings section");
        let env = texts
            .iter()
            .position(|row| row == "Env")
            .expect("Env section");
        let activity = texts
            .iter()
            .position(|row| row == "Activity")
            .expect("Activity section");

        assert!(settings < env && env < activity, "section order: {texts:?}");
        let actor = texts
            .iter()
            .find(|row| row.contains("\"BEADS_ACTOR\""))
            .expect("identity row");
        assert_eq!(
            actor, r#"  "BEADS_ACTOR"="azurite\x20""#,
            "a long neighboring key adds no padding to the identity row",
        );
        for (key, value) in [
            (r#""DISTINCT_NEWLINE""#, r#""A\nB""#),
            (r#""DISTINCT_PLAIN""#, r#""AB""#),
            (r#""DISTINCT_TAB""#, r#""a\tb""#),
            (r#""DISTINCT_UNTABBED""#, r#""ab""#),
            (r#""COMBINED_e\u{301}""#, r#""plain""#),
            (r#""FORMAT\u{202e}""#, r#""line\u{2028}separator""#),
            (r#""WIDE_\u{754c}""#, r#""wide""#),
        ] {
            let row = texts[env..activity]
                .iter()
                .find(|row| row.contains(key))
                .unwrap_or_else(|| panic!("missing escaped environment key {key:?}"));
            assert!(
                row.contains(&format!("={value}")),
                "escaped key and value must share a delimited row: {row:?}",
            );
        }
        assert!(
            texts[env..activity]
                .iter()
                .all(|row| row.is_ascii() && !row.contains(['\r', '\n'])),
            "terminal-active text reached an environment row: {:?}",
            &texts[env..activity],
        );

        let mut narrow = draw_ctx(24, 512);
        narrow.width_method = vaxis::gwidth::Method::Wcwidth;
        // Draw every row so the long ZWJ-linked value reaches RichText's
        // grapheme-width conversion. Escaping it into independent ASCII cells
        // prevents both a u8-width panic and hard-break injection.
        for row in &rows {
            let mut text = RichText::new(row.clone());
            let _ = text.draw(&narrow);
        }
        let mut overlay = ContentOverlay::new(rows[env + 1..activity - 1].to_vec());
        let surface = overlay.draw(&narrow);
        let drawn = crate::test_support::flatten(&surface)
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.char.grapheme())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let packed: String = drawn
            .chars()
            .filter(|ch| !ch.is_ascii_whitespace())
            .collect();
        assert!(
            packed.contains("\"BEADS_ACTOR\""),
            "drawn identity key: {drawn}"
        );
        assert!(
            packed.contains("\"azurite\\x20\""),
            "drawn exact value: {drawn}"
        );

        let id_without_env = without_env
            .iter()
            .map(row_text)
            .find(|row| row.starts_with("  id "))
            .expect("ordinary id row without env");
        let id_with_env = texts
            .iter()
            .find(|row| row.starts_with("  id "))
            .expect("ordinary id row with env");
        assert_eq!(
            id_with_env, &id_without_env,
            "environment layout must not change ordinary digest padding",
        );
        assert_eq!(
            continued_env_text(
                &texts[env + 1..activity - 1],
                &format!("\"{}", &long_key[..16])
            ),
            format!(
                "{}={}",
                quoted_env_text(&long_key),
                quoted_env_text("long-key-value")
            ),
            "numbered continuation rows preserve the complete long pair",
        );
    }

    /// Valid environment text at and beyond the `u16` child-height boundary is
    /// split before the real list measures it. Every escaped byte stays
    /// reconstructable, and both ends of the list remain drawable at the
    /// one-cell body width that maximizes soft-wrapped height.
    #[test]
    fn valid_u16_boundary_environment_text_remains_drawable_and_complete() {
        fn chunk_distinct_key(len: usize, tag: char) -> String {
            let mut key = String::with_capacity(len);
            let mut block = 0;
            while key.len() < len {
                let marker = format!("{tag}{block:06X}");
                let fill =
                    char::from(b'a' + u8::try_from(block % 26).expect("block remainder fits u8"));
                key.push_str(&marker);
                key.extend(std::iter::repeat_n(
                    fill,
                    ENV_ROW_PAYLOAD_CELLS - marker.len(),
                ));
                block += 1;
            }
            key.truncate(len);
            key
        }

        let at_boundary = chunk_distinct_key(65_529, 'A');
        let beyond_boundary = chunk_distinct_key(131_100, 'K');
        let env = std::collections::BTreeMap::from([
            (at_boundary.clone(), String::new()),
            (beyond_boundary.clone(), String::new()),
        ]);
        aj_session::validate_session_env(&env).expect("both unbounded pairs are valid");
        // The opening quote offsets payload boundaries by one byte from the key
        // blocks. Assert the resulting chunks are still pairwise distinct so a
        // reorder cannot hide behind equal fixture text.
        for key in [&at_boundary, &beyond_boundary] {
            let represented = format!("{}={}", quoted_env_text(key), quoted_env_text(""));
            let chunks = represented
                .as_bytes()
                .chunks(ENV_ROW_PAYLOAD_CELLS)
                .collect::<Vec<_>>();
            let unique = chunks
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                unique.len(),
                chunks.len(),
                "every payload chunk is distinct"
            );
        }
        let mut stats = sample_stats();
        stats.session_env = Some(env);

        let rows = session_info_rows(&stats, None);
        let texts: Vec<String> = rows.iter().map(row_text).collect();
        let env = texts
            .iter()
            .position(|row| row == "Env")
            .expect("Env section");
        let activity = texts
            .iter()
            .position(|row| row == "Activity")
            .expect("Activity section");
        let env_rows = &rows[env + 1..activity - 1];
        let env_texts = &texts[env + 1..activity - 1];

        for text in env_texts {
            let payload = text.split_once("] ").map_or_else(
                || text.strip_prefix("  ").expect("environment indent"),
                |(_, p)| p,
            );
            assert!(
                payload.len() <= ENV_ROW_PAYLOAD_CELLS,
                "one list child exceeded the fixed payload bound: {}",
                payload.len(),
            );
            assert!(text.is_ascii(), "terminal-active text reached a child");
        }
        for key in [&at_boundary, &beyond_boundary] {
            assert_eq!(
                continued_env_text(env_texts, &format!("\"{}", &key[..16])),
                format!("{}={}", quoted_env_text(key), quoted_env_text("")),
                "the complete pair survives continuation rows",
            );
        }

        let mut narrow = draw_ctx(2, 8);
        narrow.width_method = vaxis::gwidth::Method::Wcwidth;
        let mut overlay = ContentOverlay::new(env_rows.to_vec());
        let top = overlay.draw(&narrow);
        let body_column = |surface: &Surface| {
            crate::test_support::flatten(surface)
                .iter()
                .map(|row| row.first().expect("two-column surface").char.grapheme())
                .collect::<String>()
        };
        let expected_top: String = env_texts
            .first()
            .expect("first continuation")
            .chars()
            .take(8)
            .collect();
        assert_eq!(
            body_column(&top),
            expected_top,
            "the first continuation reaches the composited body column",
        );
        assert!(
            overlay.list.borrow().item_top_line(1)
                > u64::try_from(ENV_ROW_PAYLOAD_CELLS).expect("payload bound fits u64"),
            "the real list measured its first child at a one-cell body width",
        );
        overlay.list.borrow_mut().scroll_to_bottom();
        let bottom = overlay.draw(&narrow);
        let expected_bottom = env_texts
            .last()
            .expect("last continuation")
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        assert_eq!(
            body_column(&bottom),
            expected_bottom,
            "the final continuation payload reaches the composited body column",
        );
        assert!(
            overlay.list.borrow().is_at_bottom(),
            "the tail remains reachable"
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
        for row in session_info_rows(&sample_stats(), Some("fix-auth")) {
            assert_eq!(row.len(), 1, "plain row is one span: {row:?}");
            assert_eq!(
                row[0].style,
                Style::default(),
                "plain row default-styled: {row:?}"
            );
        }
    }

    /// The tag is the one value on this page a peer supplies, so it is folded
    /// before it becomes a row. A control character would split the row it
    /// sits on, and a lone carriage return panics `RichText`'s hard-break
    /// walk, which happens inside the draw.
    #[test]
    fn a_control_character_in_a_peer_tag_never_reaches_a_row() {
        use vaxis::vxfw::{RichText, Widget};

        let rows = session_info_rows(&sample_stats(), Some("ab\rcd"));
        let blob = rows_text(&rows);
        assert!(!blob.contains('\r'), "the label is folded: {blob:?}");
        assert!(blob.contains("abcd"), "and it is still the label: {blob:?}");
        for row in rows {
            let mut text = RichText::new(row);
            text.draw(&crate::test_support::draw_ctx(60, Some(1)));
        }
    }
}
