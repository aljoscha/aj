//! [`FilterableSelect`]: a fuzzy-filtered pick list.
//!
//! Most selector overlays are the same shape: a one-line filter input above a
//! navigable result list, where typing filters and re-ranks the rows, the
//! arrow keys move the highlight, Enter confirms the highlighted row, and
//! Escape cancels. This widget owns that mechanics, composing a
//! [`PromptInput`] filter over a [`ListView`], with a [`FuzzyMatcher`] ranking
//! the rows as the filter text changes.
//!
//! # Focus and key routing
//!
//! Focus belongs on the prompt's inner field (see
//! [`focus_target`](FilterableSelect::focus_target)) so its cursor renders
//! and printable keys edit the query. The select widget is the field's
//! ancestor on the focus path, so it intercepts the selector chords in its
//! capturing phase before the field sees them: Escape cancels, Enter (or
//! Ctrl+J) confirms, and Up/Down/Ctrl+P/Ctrl+N are forwarded to the list's
//! cursor. Everything else falls through to the field at-target.
//!
//! # Selection band
//!
//! The cursored row is drawn as a full-width band over
//! [`SelectStyles::selected_bg`] with normal foreground on top, rather than an
//! arrow-prefix marker. The row is built through a [`Source::Builder`] so
//! `item_at_idx` receives the live cursor and re-tints whichever row it lands
//! on. The list's own cursor gutter is disabled (`draw_cursor = false`); the
//! band is the whole selection cue.
//!
//! Outcomes flow through the `on_confirm` / `on_cancel` callbacks. The widget
//! owns no result state.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use crate::cell::{Cell, Character, Color, Style};
use crate::fuzzy::FuzzyMatcher;
use crate::key::{Key, Modifiers};
use crate::vxfw::{
    Builder, DrawContext, Event, EventContext, ListView, MaxSize, PromptInput, RelativePoint,
    RichText, ScrollBars, Size, Source, SubSurface, Surface, TextAlign, TextSpan, Widget,
    WidgetRef, WidthBasis,
};

/// The marker drawn before a filter overlay's query input, so the input reads
/// as a prompt. Shared by [`FilterableSelect`] and the host's settings list so
/// every text filter marks its input identically.
pub const FILTER_MARKER: &str = "> ";

/// Theme styles for the pick list's rows, threaded from the host's palette so
/// the widget carries no theme dependency of its own.
///
/// The selection band fills the cursored row's full inner width with
/// `selected_bg`. The column styles (`prefix`, `label`, `shortcut`,
/// `secondary`) get their background overpainted with `selected_bg` on the
/// banded row so the text sits on the band rather than punching a hole in it.
#[derive(Clone)]
pub struct SelectStyles {
    /// Background of the full-width band painted behind the cursored row.
    pub selected_bg: Color,
    /// Foreground for the primary label column.
    pub label: Style,
    /// Style for the right-aligned metadata column ([`SelectItem::prefix`]),
    /// typically dim.
    pub prefix: Style,
    /// Style for the leading row marker, independent of the filter prompt.
    pub row_marker: Style,
    /// Style for the key-hint column ([`SelectItem::shortcut`]), typically the
    /// keybinding-hint color, bold.
    pub shortcut: Style,
    /// Foreground for the secondary (description) column, typically dimmed.
    pub secondary: Style,
    /// Foreground of the vertical scroll-bar thumb, for selectors that show
    /// one ([`FilterableSelect::set_show_scrollbar`]). Ignored otherwise.
    pub scrollbar_thumb: Style,
    /// Style for the [`FILTER_MARKER`] drawn before the filter input.
    pub marker: Style,
}

impl Default for SelectStyles {
    /// Terminal defaults with no band, for tests and callers that render
    /// unstyled.
    fn default() -> SelectStyles {
        SelectStyles {
            selected_bg: Color::Default,
            label: Style::default(),
            prefix: Style::default(),
            row_marker: Style::default(),
            shortcut: Style::default(),
            secondary: Style::default(),
            scrollbar_thumb: Style::default(),
            marker: Style::default(),
        }
    }
}

/// An inline metadata field, aligned with the same field in every item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectColumn {
    pub text: String,
    pub alignment: TextAlign,
    /// Gap after the field. The largest request at this index applies to every row.
    pub gap_after: usize,
}

impl SelectColumn {
    /// Builds a left-aligned field.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            alignment: TextAlign::Left,
            gap_after: 2,
        }
    }

    /// Sets the gap after this field, excluding padding used for alignment.
    pub fn with_gap_after(mut self, columns: usize) -> Self {
        self.gap_after = columns;
        self
    }

    /// Sets alignment within the field's shared terminal-cell width.
    pub fn with_alignment(mut self, alignment: TextAlign) -> Self {
        self.alignment = alignment;
        self
    }
}

/// Which end of a label to omit when the row needs room for its other columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelOverflow {
    /// Keep the beginning, followed by an ellipsis.
    KeepStart,
    /// Keep the end, preceded by an ellipsis.
    KeepEnd,
}

/// One selectable row: what the list shows, what the filter matches, and an
/// optional opaque value the confirming caller can act on.
///
/// The `label` and `filter_key` are separate on purpose: the display label is
/// the human title while the filter should match a curated key (a category
/// plus a title, say) rather than the rendered columns. The widget lays out
/// the columns itself, so the label carries only the title, never padding.
///
/// A row can carry three optional columns around the label: a `prefix`
/// (right-aligned metadata column, e.g. a command category), a `shortcut` (a
/// key hint), and a `description`. The shortcut and the description share the
/// right slot, and a shortcut wins when both are set. Opt-ins add
/// a leading marker and aligned inline metadata before the label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem {
    /// Row text shown in the list.
    pub label: String,
    /// Shorten the label to leave room for the other columns. Without this
    /// opt-in, overflow clips the end of the entire row.
    pub label_overflow: Option<LabelOverflow>,
    /// Text the fuzzy filter matches and ranks against.
    pub filter_key: String,
    /// Opaque action identity, deliberately separate from rendered and
    /// searchable text. The widget never interprets it.
    pub value: Option<String>,
    /// Narrower text a scoped query matches instead of `filter_key`, for a
    /// select that offers one (see [`FilterableSelect::set_scope_sigil`]). A
    /// row without one never matches a scoped query.
    pub scope_key: Option<String>,
    /// Optional right-aligned metadata column drawn to the left of the label
    /// (a command category, say), styled with [`SelectStyles::prefix`].
    pub prefix: Option<String>,
    /// Optional key hint drawn in the right slot in
    /// [`SelectStyles::shortcut`]. Wins the slot over `description`.
    pub shortcut: Option<String>,
    /// Optional dim secondary column (a wire-level id, a one-line
    /// description), rendered after the label when no `shortcut` is set.
    pub description: Option<String>,
    /// Optional single-column leading marker in [`SelectStyles::row_marker`].
    pub marker: Option<char>,
    /// Inline metadata after the marker and prefix, before the label, styled
    /// with [`SelectStyles::secondary`]. Fields align by index across all items,
    /// including filtered-out items. Missing or empty fields reserve their
    /// column. Entirely empty columns consume no space.
    pub columns: Vec<SelectColumn>,
    /// Strike through all row text without changing colors or selection styling.
    pub strikethrough: bool,
}

impl SelectItem {
    /// Builds an item from its display label and filter key, with no extra
    /// columns.
    pub fn new(label: impl Into<String>, filter_key: impl Into<String>) -> SelectItem {
        SelectItem {
            label: label.into(),
            label_overflow: None,
            filter_key: filter_key.into(),
            value: None,
            scope_key: None,
            prefix: None,
            shortcut: None,
            description: None,
            marker: None,
            columns: Vec::new(),
            strikethrough: false,
        }
    }

    /// Shortens only the label at draw time, preserving other columns when
    /// space permits. Does not change the text used for filtering or confirmation.
    pub fn with_label_overflow(mut self, overflow: LabelOverflow) -> Self {
        self.label_overflow = Some(overflow);
        self
    }

    /// Adds an opaque action identity independent of display and search text.
    pub fn with_value(mut self, value: impl Into<String>) -> SelectItem {
        self.value = Some(value.into());
        self
    }

    /// Adds the text a scoped query matches against, for a select with a
    /// scope sigil ([`FilterableSelect::set_scope_sigil`]).
    pub fn with_scope_key(mut self, scope_key: impl Into<String>) -> SelectItem {
        self.scope_key = Some(scope_key.into());
        self
    }

    /// Adds a right-aligned metadata column drawn to the left of the label.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> SelectItem {
        self.prefix = Some(prefix.into());
        self
    }

    /// Adds a key hint drawn in the right slot. Wins the slot over a
    /// description.
    pub fn with_shortcut(mut self, shortcut: impl Into<String>) -> SelectItem {
        self.shortcut = Some(shortcut.into());
        self
    }

    /// Adds a dim secondary column, shown after the label when no shortcut is set.
    pub fn with_description(mut self, description: impl Into<String>) -> SelectItem {
        self.description = Some(description.into());
        self
    }

    /// Adds a marker before the prefix, styled with [`SelectStyles::row_marker`].
    /// The caller must supply a character that occupies one terminal column.
    /// If any item carries a marker, every row reserves its column and a
    /// one-column gap, even when filtering hides all marked items.
    pub fn with_marker(mut self, marker: char) -> SelectItem {
        self.marker = Some(marker);
        self
    }

    /// Applies strikethrough to the marker, prefix, metadata, and label.
    pub fn with_strikethrough(mut self, enabled: bool) -> SelectItem {
        self.strikethrough = enabled;
        self
    }

    /// Adds aligned inline metadata before the label.
    pub fn with_columns(mut self, columns: Vec<SelectColumn>) -> SelectItem {
        self.columns = columns;
        self
    }
}

/// The model shared between the widget, the row [`Builder`], and the filter's
/// `on_change` callback: the full item set, the filtered view onto it, the
/// matcher, and the column widths the row layout aligns to.
struct SelectState {
    items: Vec<SelectItem>,
    /// Indices into `items`, filtered and ranked best-first, each paired with
    /// its match score. Always kept in rank order: score descending, then
    /// original index ascending. Readers use only the index; the score is
    /// retained so streamed batches can be merged into the ranking without a
    /// full rescore.
    visible: Vec<(usize, u32)>,
    /// The current filter text, mirrored from the filter field on change.
    query: String,
    /// Background snapshots may follow the first result only before the user
    /// edits, navigates, or scrolls. Clearing a query does not undo interaction.
    interacted: bool,
    /// The optional narrowing sigil ([`FilterableSelect::set_scope_sigil`]).
    scope_sigil: Option<char>,
    matcher: FuzzyMatcher,
    literal_search: bool,
    /// Widest `prefix` across all items (0 when none set), the width of the
    /// right-aligned metadata column.
    prefix_width: usize,
    /// Widest `label` across all items plus [`LABEL_COLUMN_PADDING`], the
    /// width the label column pads to when a shortcut follows.
    ///
    /// Both widths come from the full item set, not the filtered view, so the
    /// columns hold a stable horizontal position as the filter narrows the
    /// visible rows.
    label_width: usize,
    /// Derived from all items so filtering cannot remove the marker gutter.
    has_marker: bool,
    /// Terminal-cell width and trailing gap, shared across the full item set.
    column_layout: Vec<(usize, usize)>,
    width_method: crate::gwidth::Method,
}

/// Gap between the right-aligned prefix column and the label.
const PREFIX_COLUMN_GAP: usize = 2;
/// Padding added past the widest label, so a shortcut in the right slot sits
/// clear of the longest label rather than flush against it.
const LABEL_COLUMN_PADDING: usize = 2;

/// Builds one banded row widget per visible index, restyling the row the
/// cursor is on. The `state` and `styles` cells are shared with the widget, so
/// a filter change or a restyle is picked up without rebuilding the builder.
struct RowBuilder {
    state: Rc<RefCell<SelectState>>,
    styles: Rc<RefCell<SelectStyles>>,
    list: Weak<RefCell<ListView>>,
}

impl Builder for RowBuilder {
    fn item_at_idx(&self, idx: usize, cursor: usize) -> Option<WidgetRef> {
        let state = self.state.borrow();
        let &(item_idx, _) = state.visible.get(idx)?;
        let styles = self.styles.borrow();
        Some(Rc::new(RefCell::new(SelectRow {
            content: build_row(&state.items[item_idx], idx == cursor, &styles, &state),
            index: item_idx,
            key: state.items[item_idx].filter_key.clone(),
            state: Rc::clone(&self.state),
            list: Weak::clone(&self.list),
        })))
    }
}

/// Row-local hit-testing excludes the filter, blank space, and scrollbar.
/// The weak list reference avoids a cycle through the list's row builder.
struct SelectRow {
    content: WidgetRef,
    index: usize,
    key: String,
    state: Rc<RefCell<SelectState>>,
    list: Weak<RefCell<ListView>>,
}

impl Widget for SelectRow {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        self.content.borrow_mut().draw(ctx)
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::Mouse(mouse) = event else { return };
        if mouse.button != crate::mouse::Button::Left || mouse.kind != crate::mouse::Type::Press {
            return;
        }
        let Some(list) = self.list.upgrade() else {
            return;
        };
        let mut state = self.state.borrow_mut();
        // An update may replace or filter out the painted row before the next
        // frame. Ignore a stale hit rather than selecting a different item.
        let Some(position) = state.visible.iter().position(|&(index, _)| {
            index == self.index && state.items[index].filter_key == self.key
        }) else {
            return;
        };
        state.interacted = true;
        list.borrow_mut().cursor = u32::try_from(position).expect("position fits u32");
        // A click selects only. Keeping focus on the filter lets typing continue,
        // and leaving scroll alone keeps the clicked row under the pointer.
        ctx.consume_and_redraw();
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// Build one row: a full-width [`RichText`] whose cells all carry
/// `selected_bg` when `selected`, so the band spans the inner width even past
/// the text. `WidthBasis::Parent` gives the surface the full list width; the
/// span backgrounds are tinted too so text cells sit on the band rather than
/// leaving default-colored holes.
///
/// Columns (left to right): an optional marker gutter, a right-aligned prefix
/// in `prefix_width` plus a gap, inline metadata, the label, then the right
/// slot. A shortcut suppresses the description and pads the label to
/// `label_width`. Otherwise, label and description have a two-cell gap.
fn build_row(
    item: &SelectItem,
    selected: bool,
    styles: &SelectStyles,
    state: &SelectState,
) -> WidgetRef {
    let prefix_width = state.prefix_width;
    let label_width = state.label_width;
    let has_marker = state.has_marker;
    let column_layout = &state.column_layout;
    let width_method = state.width_method;
    let band = selected.then_some(styles.selected_bg);
    let tint = |mut style: Style| -> Style {
        style.strikethrough |= item.strikethrough;
        if let Some(bg) = band {
            style.bg = bg;
        }
        style
    };
    let mut spans = Vec::new();
    if has_marker {
        spans.push(TextSpan {
            text: format!("{} ", item.marker.unwrap_or(' ')),
            style: tint(styles.row_marker),
            ..TextSpan::default()
        });
    }
    // Right-aligned metadata column plus its gap, only when some item carries
    // a prefix. An item without one still fills the column with spaces so the
    // label stays in its aligned position.
    if prefix_width > 0 {
        let prefix = clip_column(
            item.prefix.as_deref().unwrap_or(""),
            prefix_width,
            width_method,
        );
        let pad =
            prefix_width.saturating_sub(usize::from(crate::gwidth::gwidth(&prefix, width_method)));
        spans.push(TextSpan {
            text: format!(
                "{}{}{}",
                " ".repeat(pad),
                prefix,
                " ".repeat(PREFIX_COLUMN_GAP)
            ),
            style: tint(styles.prefix),
            ..TextSpan::default()
        });
    }
    for (index, &(width, gap)) in column_layout.iter().enumerate() {
        if width == 0 {
            continue;
        }
        let column = item.columns.get(index);
        let text = clip_column(
            column.map_or("", |column| column.text.as_str()),
            width,
            width_method,
        );
        let padding = width.saturating_sub(usize::from(crate::gwidth::gwidth(&text, width_method)));
        let left = match column.map_or(TextAlign::Left, |column| column.alignment) {
            TextAlign::Left => 0,
            TextAlign::Center => padding / 2,
            TextAlign::Right => padding,
        };
        spans.push(TextSpan {
            text: format!(
                "{}{}{}",
                " ".repeat(left),
                text,
                " ".repeat(padding - left + gap)
            ),
            style: tint(styles.secondary),
            ..TextSpan::default()
        });
    }
    let label_index = spans.len();
    if let Some(shortcut) = &item.shortcut {
        let pad = label_width.saturating_sub(usize::from(crate::gwidth::gwidth(
            &item.label,
            width_method,
        )));
        spans.push(TextSpan {
            text: format!("{}{}", item.label, " ".repeat(pad)),
            style: tint(styles.label),
            ..TextSpan::default()
        });
        spans.push(TextSpan {
            text: shortcut.clone(),
            style: tint(styles.shortcut),
            ..TextSpan::default()
        });
    } else if let Some(description) = &item.description {
        spans.push(TextSpan {
            text: item.label.clone(),
            style: tint(styles.label),
            ..TextSpan::default()
        });
        spans.push(TextSpan {
            text: "  ".to_string(),
            style: tint(styles.secondary),
            ..TextSpan::default()
        });
        spans.push(TextSpan {
            text: description.clone(),
            style: tint(styles.secondary),
            ..TextSpan::default()
        });
    } else {
        spans.push(TextSpan {
            text: item.label.clone(),
            style: tint(styles.label),
            ..TextSpan::default()
        });
    }
    let mut rich = RichText::new(spans);
    // Single-line rows: long content truncates with an ellipsis rather than
    // wrapping and pushing the list around.
    rich.softwrap = false;
    // Full inner width so the band (and its fill cells) reach the right edge.
    rich.width_basis = WidthBasis::Parent;
    if let Some(bg) = band {
        rich.base_style = Style {
            bg,
            ..Style::default()
        };
    }
    if let Some(overflow) = item.label_overflow {
        Rc::new(RefCell::new(LabelRow {
            rich,
            label_index,
            label: item.label.clone(),
            padded_width: if item.shortcut.is_some() {
                label_width
            } else {
                0
            },
            overflow,
        }))
    } else {
        Rc::new(RefCell::new(rich))
    }
}

/// The row keeps the source label because the available width can change
/// between draws. Only its label span is shortened, never markers or metadata.
struct LabelRow {
    rich: RichText,
    label_index: usize,
    label: String,
    padded_width: usize,
    overflow: LabelOverflow,
}

impl Widget for LabelRow {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let width = usize::from(ctx.max.width.expect("select rows require a bounded width"));
        let other_width: usize = self
            .rich
            .text
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != self.label_index)
            .map(|(_, span)| usize::from(crate::gwidth::gwidth(&span.text, ctx.width_method)))
            .sum();
        // If the other columns cannot fit either, retain an omission marker
        // for the label and let the row's ordinary overflow clip the remainder.
        let slot = width.saturating_sub(other_width).max(1).min(width);
        let gap = if self.padded_width > 0 {
            LABEL_COLUMN_PADDING
        } else {
            0
        };
        let budget = slot.saturating_sub(gap).max(1).min(slot);
        let mut label = match self.overflow {
            LabelOverflow::KeepStart => {
                clip_column(&self.label, budget, ctx.width_method).into_owned()
            }
            LabelOverflow::KeepEnd => clip_label_start(&self.label, budget, ctx.width_method),
        };
        let used = usize::from(crate::gwidth::gwidth(&label, ctx.width_method));
        label.push_str(&" ".repeat(self.padded_width.min(slot).saturating_sub(used)));
        self.rich.text[self.label_index].text = label;
        self.rich.draw(ctx)
    }
}

fn clip_label_start(text: &str, width: usize, method: crate::gwidth::Method) -> String {
    if usize::from(crate::gwidth::gwidth(text, method)) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let graphemes: Vec<_> = crate::unicode::grapheme_iterator(text).collect();
    let mut start = text.len();
    let mut used = 1; // Leading ellipsis.
    for grapheme in graphemes.iter().rev() {
        let part = grapheme.bytes(text);
        let cells = usize::from(crate::gwidth::gwidth(part, method));
        if used + cells > width {
            break;
        }
        start -= part.len();
        used += cells;
    }
    format!("…{}", &text[start..])
}

/// Clip metadata without splitting a grapheme or borrowing the label's cells.
fn clip_column(
    text: &str,
    width: usize,
    method: crate::gwidth::Method,
) -> std::borrow::Cow<'_, str> {
    if usize::from(crate::gwidth::gwidth(text, method)) <= width {
        return text.into();
    }
    let mut clipped = String::new();
    let mut used = 0;
    for grapheme in crate::unicode::grapheme_iterator(text) {
        let text = grapheme.bytes(text);
        let cells = usize::from(crate::gwidth::gwidth(text, method));
        if used + cells > width.saturating_sub(1) {
            break;
        }
        clipped.push_str(text);
        used += cells;
    }
    if width > 0 {
        clipped.push('…');
    }
    clipped.into()
}

/// Recomputes `visible` by scoring the full item set from scratch and resets
/// the cursor to the top so it can never point past the narrowed set. The row
/// [`Builder`] is permanent, so this only refreshes the filtered view and the
/// item count.
///
/// Used by `set_items` and by `on_change` on any non-append query change.
fn full_filter(state: &mut SelectState, list: &mut ListView) {
    let all = (0..state.items.len()).collect();
    state.visible = rank(state, all);
    list.item_count = Some(u32::try_from(state.visible.len()).expect("row count fits u32"));
    list.jump_to_item(0);
}

/// Select rows are one line tall. Preserve a visible selection's screen row,
/// or the reading anchor if the user scrolled the selection out of view. Only
/// the indices move, so input queued since the last draw is still applied.
fn retain_position(list: &mut ListView, count: usize, selected: Option<usize>, top: Option<usize>) {
    let count = u32::try_from(count).expect("row count fits u32");
    let old_top = list.scroll_top();
    let screen_row = list
        .cursor
        .checked_sub(old_top)
        .filter(|&row| row < u32::from(list.viewport_height().unwrap_or(0)));
    let cursor = selected
        .map(|pos| u32::try_from(pos).expect("pos fits u32"))
        .unwrap_or(list.cursor)
        .min(count.saturating_sub(1));
    let top = match (selected, screen_row) {
        (Some(_), Some(row)) => cursor.saturating_sub(row),
        _ => top
            .map(|pos| u32::try_from(pos).expect("pos fits u32"))
            .unwrap_or(old_top)
            .min(count.saturating_sub(1)),
    };
    list.item_count = Some(count);
    list.cursor = cursor;
    list.reanchor(top);
}

/// Rank `candidates`, indices into `items` in ascending order, against the
/// live query. Best-first, ties broken by index.
///
/// Ascending order is the caller's part of the bargain: `filter_scored`
/// breaks score ties by its own enumeration position, so handing it the
/// indices in order makes that an original-index tiebreak. That is what lets
/// a partial rescore (a narrowing query, a streamed batch) reproduce the
/// order a full rescore would have produced.
///
/// A scoped query (see [`FilterableSelect::set_scope_sigil`]) is matched
/// without its sigil against each row's `scope_key`, and a row carrying none
/// is dropped before it reaches the matcher rather than matched against an
/// empty key. Dropping it is what makes the bare sigil mean "every row that
/// has a scope key" instead of "every row".
fn rank(state: &mut SelectState, candidates: Vec<usize>) -> Vec<(usize, u32)> {
    let SelectState {
        items,
        query,
        matcher,
        scope_sigil,
        literal_search,
        ..
    } = state;
    let stripped = scope_sigil.and_then(|sigil| query.trim_start().strip_prefix(sigil));
    let scoped = stripped.is_some();
    let query = stripped.unwrap_or(query.as_str());
    let entries = candidates
        .into_iter()
        .filter(|&i| !scoped || items[i].scope_key.is_some())
        .map(|i| (i, &items[i]));
    let entries = entries.map(|(i, item)| {
        let text = if scoped {
            item.scope_key.as_deref().unwrap_or("")
        } else {
            item.filter_key.as_str()
        };
        (i, text)
    });
    if *literal_search {
        let query = crate::text_search::TextQuery::new(query);
        let mut ranked: Vec<_> = entries
            .filter_map(|(i, text)| query.score(text).map(|score| (i, score)))
            .collect();
        ranked.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
        return ranked;
    }
    matcher
        .filter_scored(entries, query, |(_, text)| *text)
        .into_iter()
        .map(|((i, _), score)| (i, score))
        .collect()
}

/// Recomputes `visible` by rescoring only the current visible subset, for a
/// query change that is a pure append. Resets the cursor to the top.
///
/// # Monotonicity invariant
///
/// When the query change is a pure append (`new.starts_with(&old) && new is
/// longer`), the new match set is a subset of the current one. Fuzzy matching
/// requires each whitespace-split token to be an ordered, case-insensitive
/// subsequence of the text, and appending to the query can only extend the
/// last token or add a token. Extending a token makes its match strictly
/// harder (a longer needle that still requires the old prefix as a
/// subsequence), and adding a token adds a requirement. Either way an item can
/// only drop out, never enter. So it is sound to rescore just the survivors.
///
/// A scope sigil does not weaken this. An append leaves the query's first
/// character alone unless the old query was empty, so the scope either stays
/// as it was or turns on from the unfiltered set, where every item is a
/// survivor already.
fn narrow_filter(state: &mut SelectState, list: &mut ListView) {
    let mut candidates: Vec<usize> = state.visible.iter().map(|&(i, _)| i).collect();
    candidates.sort_unstable();
    state.visible = rank(state, candidates);
    list.item_count = Some(u32::try_from(state.visible.len()).expect("row count fits u32"));
    list.jump_to_item(0);
}

/// Extends `visible` for a streamed batch: scores only the newly appended
/// items (`items[old_len..]`) and merges them into the existing ranking,
/// avoiding a full rescore of the accumulated set. Leaves the cursor to the
/// caller (`extend_items` restores it); only the item count is updated here.
fn merge_extend(state: &mut SelectState, list: &mut ListView, old_len: usize) {
    let tail = (old_len..state.items.len()).collect();
    let new_ranked = rank(state, tail);
    // Both lists are already in rank order (score desc, index asc), so a
    // linear two-way merge reproduces a full rescore's order. New indices all
    // exceed old ones, so a score tie between an old and a new item keeps the
    // old (lower index) first, matching the stable tiebreak.
    state.visible = merge_ranked(std::mem::take(&mut state.visible), new_ranked);
    list.item_count = Some(u32::try_from(state.visible.len()).expect("row count fits u32"));
}

/// Merge two rank-ordered lists (score descending, then index ascending) into
/// one preserving that order. Ties (equal score) keep the smaller index first.
fn merge_ranked(a: Vec<(usize, u32)>, b: Vec<(usize, u32)>) -> Vec<(usize, u32)> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let mut ai = a.into_iter().peekable();
    let mut bi = b.into_iter().peekable();
    loop {
        match (ai.peek(), bi.peek()) {
            (Some(&(a_idx, a_score)), Some(&(b_idx, b_score))) => {
                let take_a = a_score > b_score || (a_score == b_score && a_idx < b_idx);
                out.push(if take_a { ai.next() } else { bi.next() }.expect("peeked"));
            }
            (Some(_), None) => out.push(ai.next().expect("peeked")),
            (None, Some(_)) => out.push(bi.next().expect("peeked")),
            (None, None) => break,
        }
    }
    out
}

/// Tint the vertical scroll-bar thumb cells from `style`.
///
/// Applied on each draw so a runtime restyle (theme swap) is reflected
/// without rebuilding the bars. The hover and drag cells are tinted to match:
/// the bars self-stamp their surface, so they receive mouse events via bus
/// routing and the hover and drag cells are used while the thumb is hovered or
/// dragged.
fn apply_thumb_style(bars: &mut ScrollBars<ListView>, style: Style) {
    let cell = |grapheme: &str| Cell {
        char: Character::new(grapheme, 1),
        style,
        ..Cell::default()
    };
    bars.vertical_scrollbar_thumb = cell("\u{2590}");
    bars.vertical_scrollbar_hover_thumb = cell("\u{2588}");
    bars.vertical_scrollbar_drag_thumb = cell("\u{2588}");
}

/// A fuzzy-filterable select list: a [`PromptInput`] filter row, a blank
/// separator row, and a [`ListView`] of the matching rows below.
pub struct FilterableSelect {
    /// The filter input behind the [`FILTER_MARKER`] prompt marker, drawn on
    /// the top row. Its inner field is the focus target and owns the query
    /// text and its `on_change`.
    prompt: PromptInput,
    list: Rc<RefCell<ListView>>,
    /// Scroll bars wrapping the list, held behind the shared
    /// `Rc<RefCell<ScrollBars>>` handle (the list `Rc` is shared via the bars'
    /// `view`). The bars self-stamp their surface, so the vertical thumb
    /// receives mouse events through the bus. `draw` enables the vertical bar
    /// per frame only when [`Self::show_scrollbar`] is set and the list
    /// actually overflows, so a list that fits keeps the full width and shows
    /// no bar. The horizontal bar is always off (a pick list has no horizontal
    /// axis).
    bars: Rc<RefCell<ScrollBars<ListView>>>,
    /// Whether the caller wants a vertical scroll bar when the list overflows
    /// ([`Self::set_show_scrollbar`]). The bar is still hidden while the list
    /// fits.
    show_scrollbar: bool,
    label_reserve: usize,
    state: Rc<RefCell<SelectState>>,
    styles: Rc<RefCell<SelectStyles>>,
    /// Fires on Enter/Ctrl+J with the highlighted item. No-op while the
    /// filtered set is empty.
    pub on_confirm: Option<Box<dyn FnMut(&mut EventContext, &SelectItem)>>,
    /// Fires on Escape.
    pub on_cancel: Option<Box<dyn FnMut(&mut EventContext)>>,
}

impl FilterableSelect {
    /// A select over `items` styled by `styles`, initially unfiltered with the
    /// cursor on the first row.
    pub fn new(items: Vec<SelectItem>, styles: SelectStyles) -> FilterableSelect {
        let initial = SelectState {
            visible: Vec::new(),
            items,
            query: String::new(),
            interacted: false,
            scope_sigil: None,
            matcher: FuzzyMatcher::new(),
            literal_search: false,
            prefix_width: 0,
            label_width: 0,
            has_marker: false,
            column_layout: Vec::new(),
            width_method: crate::gwidth::Method::Unicode,
        };
        let state = Rc::new(RefCell::new(initial));
        let styles = Rc::new(RefCell::new(styles));
        let mut list_view = ListView::new(Source::default());
        // The band replaces the arrow gutter, so the list draws no cursor
        // indicator of its own.
        list_view.draw_cursor = false;

        // Wrap the list in scroll bars for the vertical thumb. The bars own
        // the list behind their shared `view` handle, which we keep a clone
        // of for the widget's own accessors. A pick list has no horizontal
        // axis, and the vertical bar is opt-in so selectors that don't want
        // it stay pixel-identical (`draw` reserves no column while it is off).
        let bars = ScrollBars::new(list_view);
        bars.borrow_mut().draw_horizontal_scrollbar = false;
        bars.borrow_mut().draw_vertical_scrollbar = false;
        let list = Rc::clone(&bars.borrow().view);
        list.borrow_mut().children = Source::Builder(Box::new(RowBuilder {
            state: Rc::clone(&state),
            styles: Rc::clone(&styles),
            list: Rc::downgrade(&list),
        }));
        full_filter(&mut state.borrow_mut(), &mut list.borrow_mut());

        let prompt = PromptInput::new(FILTER_MARKER, styles.borrow().marker);
        {
            let state = Rc::clone(&state);
            let list = Rc::clone(&list);
            // NOTE: this fires from the inner field's own handle_event, at
            // which point the select's capturing borrow has already been
            // released, so borrowing the shared state and list here cannot
            // collide with the widget's own borrows.
            prompt.set_on_change(move |ctx, text| {
                let mut state = state.borrow_mut();
                let mut list = list.borrow_mut();
                // A pure append can only shrink the match set (see
                // `narrow_filter`'s monotonicity invariant), so rescore just
                // the current survivors. Any other edit (backspace, paste,
                // mid-string change) may add matches, so rescore everything.
                let is_append = text.starts_with(&state.query) && text.len() > state.query.len();
                state.query = text.to_string();
                state.interacted = true;
                // Quote delimiters are syntax in literal search. Keep its
                // edits outside the fuzzy-token monotonicity assumption.
                if is_append && !state.literal_search {
                    narrow_filter(&mut state, &mut list);
                } else {
                    full_filter(&mut state, &mut list);
                }
                ctx.redraw = true;
            });
        }

        FilterableSelect {
            prompt,
            list,
            bars,
            show_scrollbar: false,
            label_reserve: 0,
            state,
            styles,
            on_confirm: None,
            on_cancel: None,
        }
    }

    /// Opt into a vertical scroll bar for a list that can overflow. Off by
    /// default. Even when on, `draw` shows the bar only while the list has
    /// more rows than fit, so a short or filtered-down list keeps the full
    /// width and no bar. Wheel and arrow keys drive the scroll (the thumb is
    /// a position indicator, not a drag handle), matching the read-only
    /// content overlays.
    pub fn set_show_scrollbar(&mut self, show: bool) {
        self.show_scrollbar = show;
    }

    /// Use case-insensitive literal substrings instead of fuzzy subsequences.
    /// All whitespace-separated terms must occur, in any order. Double quotes
    /// group a contiguous phrase, including while the closing quote is absent.
    /// A contiguous match of the full query's terms ranks first, with source
    /// order breaking ties. Apostrophes and other punctuation are literal.
    pub fn set_literal_search(&mut self, enabled: bool) {
        let mut state = self.state.borrow_mut();
        state.literal_search = enabled;
        full_filter(&mut state, &mut self.list.borrow_mut());
    }

    /// Reserve label cells by clipping the widest leading metadata fields first.
    /// The reserve is capped at half the row width after the marker and scrollbar,
    /// rounded up, and at the widest visible label's actual needs. Zero (the
    /// default) leaves metadata at its natural width. Metadata widths are shared
    /// across rows, including filtered-out items, within that budget.
    pub fn set_label_reserve(&mut self, cells: usize) {
        self.label_reserve = cells;
    }

    /// Offer a one-character query scope: a query whose first character is
    /// `sigil` matches rows' [`SelectItem::scope_key`] with the sigil
    /// stripped, and rows that carry no scope key drop out entirely. So the
    /// bare sigil lists every row that has one.
    ///
    /// Off by default, and the sigil anywhere but the front of the query is
    /// ordinary text matched against `filter_key` like any other character.
    /// Takes effect on the next filter, so set it before the select is used.
    pub fn set_scope_sigil(&mut self, sigil: char) {
        self.state.borrow_mut().scope_sigil = Some(sigil);
    }

    /// The widget the host should focus while this select is active: the
    /// filter field, so its cursor renders and printables edit the query.
    pub fn focus_target(&self) -> WidgetRef {
        self.prompt.focus_target()
    }

    /// The current filter text.
    pub fn query(&self) -> String {
        self.state.borrow().query.clone()
    }

    /// Replace the row styles (a runtime theme swap). The row [`Builder`]
    /// reads the shared cell, so the next draw re-tints without a rebuild.
    pub fn set_styles(&self, styles: SelectStyles) {
        *self.styles.borrow_mut() = styles;
    }

    /// Replace the item set and re-apply the active filter, keeping the
    /// filter field and list widgets (so focus survives). Resets the
    /// cursor to the top. Used when the row source changes wholesale (a
    /// scope toggle, or an async fill).
    pub fn set_items(&self, items: Vec<SelectItem>) {
        let mut state = self.state.borrow_mut();
        state.items = items;
        state.interacted = false;
        full_filter(&mut state, &mut self.list.borrow_mut());
    }

    /// Replace a ranked snapshot. An untouched, empty query follows the first
    /// result. After editing, navigation, or scrolling, retain the selected row
    /// and its screen position while it survives. When the selection is offscreen,
    /// retain the top visible row instead. Removed anchors fall back to the
    /// nearest rank.
    /// Callers must supply unique, stable filter keys. Query edits and
    /// `set_items` explicitly reset selection to the best result.
    pub fn set_ranked_items(&self, items: Vec<SelectItem>) {
        let mut state = self.state.borrow_mut();
        if state.items == items {
            return;
        }
        let mut list = self.list.borrow_mut();
        if !state.interacted && state.query.is_empty() {
            state.items = items;
            full_filter(&mut state, &mut list);
            return;
        }
        let key_at = |pos: u32| {
            state
                .visible
                .get(usize::try_from(pos).expect("position fits usize"))
                .map(|&(i, _)| state.items[i].filter_key.clone())
        };
        let selected = key_at(list.cursor);
        let top = key_at(list.scroll_top());
        state.items = items;
        let all = (0..state.items.len()).collect();
        state.visible = rank(&mut state, all);
        let position = |key: Option<String>| {
            key.and_then(|key| {
                state
                    .visible
                    .iter()
                    .position(|&(i, _)| state.items[i].filter_key == key)
            })
        };
        retain_position(
            &mut list,
            state.visible.len(),
            position(selected),
            position(top),
        );
    }

    /// Append `items` to the row set and re-apply the active filter,
    /// keeping the widgets, the cursor, and the scroll position. Used to
    /// stream in batches of an incremental scan without clearing or
    /// re-anchoring what already showed.
    ///
    /// Appended items take fresh indices past the existing ones, so every
    /// row already on screen keeps its index, visible position, and scroll
    /// anchor. We deliberately touch neither the cursor nor the scroll: a
    /// batch that lands while the user is scrolled partway down must not
    /// yank the view. `merge_extend` only grows the item count and merges
    /// the new tail into the ranking.
    pub fn extend_items(&self, items: Vec<SelectItem>) {
        let mut state = self.state.borrow_mut();
        let old_len = state.items.len();
        state.items.extend(items);
        // Score only the new tail and merge it into the ranking, rather
        // than rescoring the whole accumulated set.
        merge_extend(&mut state, &mut self.list.borrow_mut(), old_len);
    }

    /// Replace the item at `index` and re-rank, keeping the highlight on the
    /// row it was on and the scroll where it was. Used when a row's text
    /// arrives after the list is on screen: the row set is unchanged, so
    /// nothing the user is looking at should move. Under a live query the
    /// changed row can move in the ranking, and a visible highlight retains
    /// its screen row whenever the list bounds allow it. Out-of-range
    /// indices are ignored.
    pub fn update_item(&self, index: usize, item: SelectItem) {
        let mut state = self.state.borrow_mut();
        if index >= state.items.len() {
            return;
        }
        let mut list = self.list.borrow_mut();
        let highlighted = state
            .visible
            .get(usize::try_from(list.cursor).expect("cursor fits usize"))
            .map(|&(i, _)| i);
        let top = state
            .visible
            .get(usize::try_from(list.scroll_top()).expect("top fits usize"))
            .map(|&(i, _)| i);
        state.items[index] = item;
        let all = (0..state.items.len()).collect();
        state.visible = rank(&mut state, all);
        let position = |index: Option<usize>| {
            index.and_then(|i| state.visible.iter().position(|&(j, _)| j == i))
        };
        retain_position(
            &mut list,
            state.visible.len(),
            position(highlighted),
            position(top),
        );
    }

    /// Move the cursor onto the first visible item matching `pred`, used to
    /// pre-select the currently-active row on open. Returns whether a match
    /// was found and the cursor moved. A no-op returning `false` when
    /// nothing visible matches.
    pub fn select_matching(&self, pred: impl Fn(&SelectItem) -> bool) -> bool {
        let pos = {
            let state = self.state.borrow();
            state
                .visible
                .iter()
                .position(|&(i, _)| pred(&state.items[i]))
        };
        if let Some(pos) = pos {
            self.state.borrow_mut().interacted = true;
            self.list
                .borrow_mut()
                .jump_to_item(u32::try_from(pos).expect("pos fits u32"));
            true
        } else {
            false
        }
    }

    /// Display labels of the filtered rows, ranked best-first.
    pub fn visible_labels(&self) -> Vec<String> {
        let state = self.state.borrow();
        state
            .visible
            .iter()
            .map(|&(i, _)| state.items[i].label.clone())
            .collect()
    }

    /// The highlighted item, or `None` while the filtered set is empty.
    pub fn selected(&self) -> Option<SelectItem> {
        let cursor = usize::try_from(self.list.borrow().cursor).expect("cursor fits usize");
        let state = self.state.borrow();
        state
            .visible
            .get(cursor)
            .map(|&(i, _)| state.items[i].clone())
    }
}

impl Widget for FilterableSelect {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        {
            let mut state = self.state.borrow_mut();
            let mut layout = Vec::<(usize, usize)>::new();
            let mut prefix_width = 0;
            let mut label_width = 0;
            let mut has_marker = false;
            // Label width only controls shortcut alignment. Trailing previews
            // can be long and need no full-text measurement for this layout.
            let has_shortcut = state.items.iter().any(|item| item.shortcut.is_some());
            for item in &state.items {
                prefix_width =
                    prefix_width.max(ctx.string_width(item.prefix.as_deref().unwrap_or("")));
                if has_shortcut {
                    label_width = label_width.max(ctx.string_width(&item.label));
                }
                has_marker |= item.marker.is_some();
                layout.resize(layout.len().max(item.columns.len()), (0, 0));
                for (index, column) in item.columns.iter().enumerate() {
                    let (width, gap) = &mut layout[index];
                    *width = (*width).max(ctx.string_width(&column.text));
                    *gap = (*gap).max(column.gap_after);
                }
            }
            if self.label_reserve > 0 {
                let size = ctx.max.size();
                let scrollbar = self.show_scrollbar
                    && state.visible.len() > usize::from(size.height.saturating_sub(2));
                let available = usize::from(size.width)
                    .saturating_sub(usize::from(scrollbar))
                    .saturating_sub(if has_marker { 2 } else { 0 });
                let cap = self.label_reserve.min(available.div_ceil(2));
                let mut reserve = 0;
                // Short labels do not need empty reserved cells at the cost of
                // clipped metadata. Stop measuring once the reserve is filled,
                // so long prompts do not require full-text width scans.
                for &(index, _) in &state.visible {
                    let label = &state.items[index].label;
                    let mut width = 0;
                    for grapheme in crate::unicode::grapheme_iterator(label) {
                        width = (width + ctx.string_width(grapheme.bytes(label))).min(cap);
                        if width == cap {
                            break;
                        }
                    }
                    reserve = reserve.max(width);
                    if reserve == cap {
                        break;
                    }
                }
                let budget = available - reserve;
                // Treat the prefix like the inline fields for budgeting, while
                // keeping its own style and alignment. Short fields keep their
                // natural widths, so one long host or tag cannot crowd them out.
                let mut fields = vec![(prefix_width, PREFIX_COLUMN_GAP)];
                fields.extend_from_slice(&layout);
                let mut used: usize = fields
                    .iter()
                    .filter(|(width, _)| *width > 0)
                    .map(|(width, gap)| width + gap)
                    .sum();
                while used > budget {
                    let (width, gap) = fields
                        .iter_mut()
                        .max_by_key(|(width, _)| *width)
                        .expect("the prefix slot exists");
                    *width -= 1;
                    used -= 1;
                    if *width == 0 {
                        used -= *gap;
                    }
                }
                prefix_width = fields[0].0;
                layout.copy_from_slice(&fields[1..]);
            }
            state.prefix_width = prefix_width;
            state.label_width = label_width + LABEL_COLUMN_PADDING;
            state.has_marker = has_marker;
            state.column_layout = layout;
            state.width_method = ctx.width_method;
        }
        let size = ctx.max.size();
        let mut surface = Surface::with_size(size);

        let filter_ctx = ctx.with_constraints(
            Size {
                width: 0,
                height: 0,
            },
            MaxSize {
                width: Some(size.width),
                height: Some(1),
            },
        );
        // Keep the marker tinted with the live styles so a theme swap
        // re-colors it without rebuilding the prompt. Drawn directly rather
        // than via `draw_widget`: the prompt's own identity is unused, the
        // focus target is the field it stamps inside. The bars are also drawn
        // directly, but they self-stamp their identity, so mouse events reach
        // them.
        self.prompt.marker_style = self.styles.borrow().marker;
        surface.children.push(SubSurface {
            origin: RelativePoint { row: 0, col: 0 },
            surface: self.prompt.draw(&filter_ctx),
            z_index: 0,
        });

        // Row 1 stays blank as the filter/list separator.
        let list_height = size.height.saturating_sub(2);
        if list_height > 0 {
            let list_ctx = ctx.with_constraints(
                Size {
                    width: 0,
                    height: 0,
                },
                MaxSize {
                    width: Some(size.width),
                    height: Some(list_height),
                },
            );
            // Show the vertical bar only when the caller opted in and the
            // list overflows the viewport. Rows are single-line
            // (softwrap off), so overflow is exactly "more visible rows than
            // fit", knowable here without a trial draw and stable under the
            // one-column narrowing the bar adds (the row count can't change).
            let overflow = self.state.borrow().visible.len() > usize::from(list_height);
            // Calling `draw` while holding the `borrow_mut` is safe: the bars'
            // self-stamp uses `Weak::upgrade`, which does not borrow the
            // `RefCell`.
            let bars_surface = {
                let mut bars = self.bars.borrow_mut();
                bars.draw_vertical_scrollbar = self.show_scrollbar && overflow;
                // Tint the thumb from the live styles so a runtime restyle
                // (theme swap) is reflected without rebuilding the bars. The
                // bars draw the inner list (stamping its identity for wheel/key
                // routing) and reserve the rightmost column for the thumb only
                // while the vertical bar is enabled.
                apply_thumb_style(&mut bars, self.styles.borrow().scrollbar_thumb);
                bars.draw(&list_ctx)
            };
            surface.children.push(SubSurface {
                origin: RelativePoint { row: 2, col: 0 },
                surface: bars_surface,
                z_index: 0,
            });
        }
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        if let Event::Mouse(mouse) = event
            && matches!(
                mouse.button,
                crate::mouse::Button::WheelUp | crate::mouse::Button::WheelDown
            )
        {
            self.state.borrow_mut().interacted = true;
        }
        // Focus sits on the filter field, so the selector chords are
        // intercepted here in the capturing phase, before the field's
        // at-target handling (Enter would otherwise clear the field, and
        // the field has no Escape or Up/Down bindings to shadow).
        let Event::KeyPress(key) = event else {
            return;
        };
        if key.matches(Key::ESCAPE, Modifiers::empty()) {
            if let Some(cb) = self.on_cancel.as_mut() {
                cb(ctx);
            }
            ctx.consume_and_redraw();
            return;
        }
        if key.matches(Key::ENTER, Modifiers::empty())
            || key.matches(u32::from('j'), Modifiers::CTRL)
        {
            // Clone the item out before firing so the callback is free to
            // re-enter accessors that borrow the shared state.
            if let Some(item) = self.selected()
                && let Some(cb) = self.on_confirm.as_mut()
            {
                cb(ctx, &item);
            }
            ctx.consume_and_redraw();
            return;
        }
        if key.matches(Key::DOWN, Modifiers::empty())
            || key.matches(u32::from('n'), Modifiers::CTRL)
        {
            self.state.borrow_mut().interacted = true;
            self.list.borrow_mut().next_item(ctx);
            return;
        }
        if key.matches(Key::UP, Modifiers::empty()) || key.matches(u32::from('p'), Modifiers::CTRL)
        {
            self.state.borrow_mut().interacted = true;
            self.list.borrow_mut().prev_item(ctx);
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gwidth;

    fn key(codepoint: u32, mods: Modifiers) -> Event {
        Event::KeyPress(Key {
            codepoint,
            mods,
            ..Key::default()
        })
    }

    fn typed(c: char) -> Event {
        Event::KeyPress(Key {
            codepoint: u32::from(c),
            text: Some(c.to_string().into()),
            ..Key::default()
        })
    }

    /// Emulates the focus-path dispatch for a select whose filter field is
    /// focused: the select captures first, then the field handles at-target
    /// if the event was not consumed.
    fn send(select: &mut FilterableSelect, event: &Event) {
        let mut ctx = EventContext::new();
        ctx.phase = crate::vxfw::Phase::Capturing;
        select.capture_event(&mut ctx, event);
        if !ctx.consume_event {
            ctx.phase = crate::vxfw::Phase::AtTarget;
            select
                .focus_target()
                .borrow_mut()
                .handle_event(&mut ctx, event);
        }
    }

    fn sample() -> FilterableSelect {
        FilterableSelect::new(
            vec![
                SelectItem::new("alpha", "alpha"),
                SelectItem::new("bravo", "bravo"),
                SelectItem::new("charlie", "charlie"),
            ],
            SelectStyles::default(),
        )
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
            width_method: gwidth::Method::Unicode,
        }
    }

    #[test]
    fn filterable_select() {
        let mut select = sample();
        assert_eq!(select.visible_labels(), ["alpha", "bravo", "charlie"]);
        assert_eq!(select.selected().map(|i| i.label), Some("alpha".into()));

        // Typing narrows the rows fuzzily and keeps the cursor in bounds.
        for c in "br".chars() {
            send(&mut select, &typed(c));
        }
        assert_eq!(select.query(), "br");
        assert_eq!(select.visible_labels(), ["bravo"]);
        assert_eq!(select.selected().map(|i| i.label), Some("bravo".into()));

        // Deleting the query restores the full set.
        for _ in 0..2 {
            send(&mut select, &key(Key::BACKSPACE, Modifiers::empty()));
        }
        assert_eq!(select.visible_labels().len(), 3);
    }

    #[test]
    fn navigation_keys_move_the_cursor_and_printables_edit_the_filter() {
        let mut select = sample();

        send(&mut select, &key(Key::DOWN, Modifiers::empty()));
        assert_eq!(select.selected().map(|i| i.label), Some("bravo".into()));
        send(&mut select, &key(u32::from('n'), Modifiers::CTRL));
        assert_eq!(select.selected().map(|i| i.label), Some("charlie".into()));
        send(&mut select, &key(u32::from('p'), Modifiers::CTRL));
        send(&mut select, &key(Key::UP, Modifiers::empty()));
        assert_eq!(select.selected().map(|i| i.label), Some("alpha".into()));
        // Navigation left the filter untouched.
        assert_eq!(select.query(), "");

        // A bare `j`/`k` is typing, not navigation (the list is never
        // focused, so its vi bindings are unreachable by design).
        send(&mut select, &typed('l'));
        assert_eq!(select.query(), "l");
        let visible = select.visible_labels();
        assert_eq!(visible.len(), 2, "alpha and charlie match: {visible:?}");
        assert!(!visible.contains(&"bravo".to_string()));
        // The narrowed set reset the cursor to the top.
        assert_eq!(select.selected().map(|i| i.label), Some(visible[0].clone()));
    }

    #[test]
    fn literal_search_edits_and_streamed_rows_keep_phrase_ranking() {
        let mut select = FilterableSelect::new(
            items(&["footer then editor", "ed_itor foot_er", "EDITOR footer"]),
            SelectStyles::default(),
        );
        select.set_literal_search(true);
        for c in "editor footer".chars() {
            send(&mut select, &typed(c));
        }
        assert_eq!(
            select.visible_labels(),
            ["EDITOR footer", "footer then editor"]
        );
        select.extend_items(items(&[
            "another editor footer",
            "footer and editor",
            "nothing",
        ]));
        assert_eq!(
            select.visible_labels(),
            [
                "EDITOR footer",
                "another editor footer",
                "footer then editor",
                "footer and editor"
            ]
        );

        for _ in 0.."editor footer".len() {
            send(&mut select, &key(Key::BACKSPACE, Modifiers::empty()));
        }
        assert_eq!(select.visible_labels().len(), 6);
        for c in "\"editor footer\"".chars() {
            send(&mut select, &typed(c));
        }
        assert_eq!(
            select.visible_labels(),
            ["EDITOR footer", "another editor footer"]
        );
        select.set_items(items(&[
            "footer editor",
            "new editor footer",
            "editor gap footer",
        ]));
        assert_eq!(select.visible_labels(), ["new editor footer"]);
    }

    #[test]
    fn literal_search_handles_punctuation_unicode_and_mixed_phrases() {
        for (query, expected) in [
            (
                "",
                vec![
                    "ÄPFEL: here's a thought",
                    "here's a different thought about äpfel",
                    "not a match",
                ],
            ),
            (
                "  \"\"  ",
                vec![
                    "ÄPFEL: here's a thought",
                    "here's a different thought about äpfel",
                    "not a match",
                ],
            ),
            (
                "äpfel \"here's a thought\"",
                vec!["ÄPFEL: here's a thought"],
            ),
            ("äpfel: thought", vec!["ÄPFEL: here's a thought"]),
            (
                "thought ÄPFEL",
                vec![
                    "ÄPFEL: here's a thought",
                    "here's a different thought about äpfel",
                ],
            ),
        ] {
            let mut select = FilterableSelect::new(
                items(&[
                    "ÄPFEL: here's a thought",
                    "here's a different thought about äpfel",
                    "not a match",
                ]),
                SelectStyles::default(),
            );
            select.set_literal_search(true);
            for c in query.chars() {
                send(&mut select, &typed(c));
            }
            assert_eq!(select.visible_labels(), expected, "query: {query}");
        }
    }

    #[test]
    fn enter_confirms_the_highlighted_item() {
        let mut select = sample();
        let picked: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let sink = Rc::clone(&picked);
        select.on_confirm = Some(Box::new(move |_ctx, item| {
            *sink.borrow_mut() = Some(item.label.clone());
        }));

        send(&mut select, &key(Key::DOWN, Modifiers::empty()));
        send(&mut select, &key(Key::ENTER, Modifiers::empty()));
        assert_eq!(picked.borrow().as_deref(), Some("bravo"));
    }

    #[test]
    fn enter_with_no_match_confirms_nothing_but_is_consumed() {
        let mut select = sample();
        let fired = Rc::new(RefCell::new(false));
        let sink = Rc::clone(&fired);
        select.on_confirm = Some(Box::new(move |_ctx, _item| *sink.borrow_mut() = true));

        for c in "zzz".chars() {
            send(&mut select, &typed(c));
        }
        assert!(select.visible_labels().is_empty());
        send(&mut select, &key(Key::ENTER, Modifiers::empty()));
        assert!(!*fired.borrow());
        // The consumed Enter never reached the field, so the query survives.
        assert_eq!(select.query(), "zzz");
    }

    #[test]
    fn escape_cancels() {
        let mut select = sample();
        let cancelled = Rc::new(RefCell::new(false));
        let sink = Rc::clone(&cancelled);
        select.on_cancel = Some(Box::new(move |_ctx| *sink.borrow_mut() = true));
        send(&mut select, &key(Key::ESCAPE, Modifiers::empty()));
        assert!(*cancelled.borrow());
    }

    #[test]
    fn draw_lays_out_filter_separator_and_list() {
        let mut select = sample();
        let surface = select.draw(&draw_ctx(30, 10));
        assert_eq!(surface.size.width, 30);
        assert_eq!(surface.size.height, 10);
        assert_eq!(surface.children.len(), 2);
        // Filter row on top, list below the blank separator row.
        assert_eq!(surface.children[0].origin, RelativePoint { row: 0, col: 0 });
        assert_eq!(surface.children[0].surface.size.height, 1);
        assert_eq!(surface.children[1].origin, RelativePoint { row: 2, col: 0 });
        assert_eq!(surface.children[1].surface.size.height, 8);
    }

    /// The filter row is drawn behind the shared `> ` marker, and the field
    /// (which owns the query and cursor) is shifted right past it.
    #[test]
    fn filter_row_carries_the_prompt_marker() {
        let mut select = sample();
        let surface = select.draw(&draw_ctx(30, 10));
        // children[0] is the prompt wrapper; its own buffer holds the marker
        // and its child is the field shifted past it.
        let prompt = &surface.children[0].surface;
        let marker: String = prompt.buffer[..FILTER_MARKER.chars().count()]
            .iter()
            .map(|c| c.char.grapheme())
            .collect();
        assert_eq!(marker, FILTER_MARKER);
        assert_eq!(
            prompt.children[0].origin,
            RelativePoint {
                row: 0,
                col: i32::try_from(FILTER_MARKER.chars().count()).unwrap(),
            },
        );

        // A runtime restyle re-tints the marker on the next draw.
        let swapped = Color::Index(41);
        select.set_styles(SelectStyles {
            marker: Style {
                fg: swapped,
                ..Style::default()
            },
            ..SelectStyles::default()
        });
        let surface = select.draw(&draw_ctx(30, 10));
        assert_eq!(
            surface.children[0].surface.buffer[0].style.fg, swapped,
            "set_styles re-tints the marker on the next draw"
        );
    }

    /// The E-7 band: the cursored row's cells carry `selected_bg` across the
    /// full inner width, non-selected rows do not, and navigation moves the
    /// band down with the cursor.
    #[test]
    fn selected_row_is_a_full_width_band() {
        let band = Color::Index(4);
        let mut select = FilterableSelect::new(
            vec![
                SelectItem::new("alpha", "alpha").with_description("first"),
                SelectItem::new("bravo", "bravo").with_description("second"),
                SelectItem::new("charlie", "charlie"),
            ],
            SelectStyles {
                selected_bg: band,
                label: Style::default(),
                prefix: Style::default(),
                row_marker: Style::default(),
                shortcut: Style::default(),
                secondary: Style::default(),
                scrollbar_thumb: Style::default(),
                marker: Style::default(),
            },
        );

        let row_bgs = |select: &mut FilterableSelect| -> Vec<Vec<Color>> {
            let surface = select.draw(&draw_ctx(30, 10));
            // The scroll-bars wrapper sits at row 2; its first child is the
            // inner list, whose children are the row sub-surfaces.
            let bars = &surface.children[1].surface;
            let list = &bars.children[0].surface;
            // The list wraps each row in a child sub-surface; flatten one
            // level to read the row cells.
            list.children
                .iter()
                .map(|row| {
                    let width = usize::from(row.surface.size.width);
                    (0..width)
                        .map(|col| {
                            row.surface
                                .buffer
                                .get(col)
                                .map(|c| c.style.bg)
                                .unwrap_or(Color::Default)
                        })
                        .collect()
                })
                .collect()
        };

        let bgs = row_bgs(&mut select);
        // First row (cursored) is banded edge to edge; it spans the full
        // 30-column inner width.
        assert_eq!(bgs[0].len(), 30, "banded row spans the inner width");
        assert!(
            bgs[0].iter().all(|c| *c == band),
            "cursored row fully banded: {:?}",
            bgs[0]
        );
        // Other rows carry no band.
        assert!(
            bgs[1].iter().all(|c| *c == Color::Default),
            "non-cursored row unbanded: {:?}",
            bgs[1]
        );

        // Moving the cursor moves the band.
        send(&mut select, &key(Key::DOWN, Modifiers::empty()));
        let bgs = row_bgs(&mut select);
        assert!(
            bgs[0].iter().all(|c| *c == Color::Default),
            "band left the first row: {:?}",
            bgs[0]
        );
        assert!(
            bgs[1].iter().all(|c| *c == band),
            "band moved to the second row: {:?}",
            bgs[1]
        );
    }

    /// Draw the select and return each visible row's laid-out cells. Rows are
    /// full-width (`WidthBasis::Parent`), so a row is `width` cells including
    /// the trailing fill.
    fn row_cells(select: &mut FilterableSelect, width: u16, height: u16) -> Vec<Vec<Cell>> {
        let surface = select.draw(&draw_ctx(width, height));
        // The scroll-bars wrapper sits at row 2; its first child is the inner
        // list, whose children are the row sub-surfaces.
        let bars = &surface.children[1].surface;
        let list = &bars.children[0].surface;
        list.children
            .iter()
            .map(|row| row.surface.buffer.clone())
            .collect()
    }

    /// The row's graphemes concatenated, for locating a column by its text.
    fn row_text(cells: &[Cell]) -> String {
        cells.iter().map(|c| c.char.grapheme()).collect()
    }

    #[test]
    fn label_overflow_keeps_whole_graphemes_and_recomputes_on_resize() {
        let label = "abcdef/e\u{301}界";
        let mut select = FilterableSelect::new(
            vec![SelectItem::new(label, label).with_label_overflow(LabelOverflow::KeepEnd)],
            SelectStyles::default(),
        );
        for (width, expected) in [
            (4, "…e\u{301}界"),
            (3, "…界"),
            (2, "…"),
            (1, "…"),
            (10, label),
            (4, "…e\u{301}界"),
        ] {
            let cells = row_cells(&mut select, width, 4).remove(0);
            // Wide-character continuation cells and unused cells carry blanks.
            let text: String = cells
                .iter()
                .filter(|cell| cell.char.grapheme() != " ")
                .map(|cell| cell.char.grapheme())
                .collect();
            assert_eq!(text, expected, "width {width}");
        }
        let mut emoji = FilterableSelect::new(
            vec![SelectItem::new("long/👩‍💻x", "key").with_label_overflow(LabelOverflow::KeepEnd)],
            SelectStyles::default(),
        );
        let cells = row_cells(&mut emoji, 4, 4).remove(0);
        assert!(cells.iter().any(|cell| cell.char.grapheme() == "👩‍💻"));
        assert!(
            row_text(&row_cells(&mut emoji, 3, 4)[0])
                .trim_end()
                .ends_with('x')
        );
        assert!(!row_text(&row_cells(&mut emoji, 3, 4)[0]).contains('👩'));
    }

    #[test]
    fn label_overflow_preserves_surrounding_columns_and_default_rows() {
        let item = SelectItem::new("long/path/tail", "key")
            .with_marker('*')
            .with_columns(vec![SelectColumn::new("7")])
            .with_description("down");
        let mut select = FilterableSelect::new(
            vec![
                item.clone().with_label_overflow(LabelOverflow::KeepEnd),
                item.clone().with_label_overflow(LabelOverflow::KeepStart),
                item,
            ],
            SelectStyles::default(),
        );
        let rows = row_cells(&mut select, 17, 6);
        assert_eq!(row_text(&rows[0]), "* 7  …/tail  down");
        assert_eq!(row_text(&rows[1]), "* 7  long/…  down");
        assert_eq!(row_text(&rows[2]), "* 7  long/path/t…");

        let mut shortcut = FilterableSelect::new(
            vec![
                SelectItem::new("long/path/tail", "key")
                    .with_shortcut("Enter")
                    .with_label_overflow(LabelOverflow::KeepEnd),
            ],
            SelectStyles::default(),
        );
        assert_eq!(row_text(&row_cells(&mut shortcut, 10, 4)[0]), "…il  Enter");
        assert_eq!(
            row_text(&row_cells(&mut shortcut, 21, 4)[0]),
            "long/path/tail  Enter"
        );
    }

    #[test]
    fn label_reserve_accounts_for_marker_scrollbar_and_wide_metadata() {
        let item = SelectItem::new("p".repeat(100), "search")
            .with_marker('*')
            .with_prefix("e\u{301}界".repeat(20))
            .with_columns(vec![
                SelectColumn::new("界e\u{301}".repeat(20)),
                SelectColumn::new("7"),
            ]);
        let mut select = FilterableSelect::new(vec![item.clone(), item], SelectStyles::default());
        select.set_label_reserve(12);
        select.set_show_scrollbar(true);
        for width in 10..=80 {
            // One visible row forces a scrollbar. The marker takes two cells
            // and the scrollbar takes one, neither belongs to the label budget.
            let rows = row_cells(&mut select, width, 3);
            let row = &rows[0];
            assert_eq!(row.len(), usize::from(width - 1));
            let reserve = 12.min(usize::from(width - 3).div_ceil(2));
            let shown = row
                .iter()
                .filter(|cell| cell.char.grapheme() == "p")
                .count();
            assert!(shown >= reserve - 1, "width={width}: {}", row_text(row));
            assert_eq!(row.last().unwrap().char.grapheme(), "…");
        }
    }

    #[test]
    fn inline_columns_align_and_keep_metadata_when_preview_is_clipped() {
        let dim = Style {
            dim: true,
            fg: Color::Index(8),
            ..Style::default()
        };
        let bright = Style {
            fg: Color::Index(15),
            ..Style::default()
        };
        let band = Color::Index(4);
        let mut select = FilterableSelect::new(
            vec![
                SelectItem::new("preview alpha is long", "alpha")
                    .with_marker('*')
                    .with_prefix("界x")
                    .with_columns(vec![
                        SelectColumn::new("1h").with_gap_after(1),
                        SelectColumn::new(""),
                        SelectColumn::new("123").with_alignment(TextAlign::Right),
                        SelectColumn::new("界"),
                    ]),
                SelectItem::new("preview bravo is long", "bravo")
                    .with_prefix("tag")
                    .with_columns(vec![
                        SelectColumn::new("20min"),
                        SelectColumn::new(""),
                        SelectColumn::new("4").with_alignment(TextAlign::Right),
                        SelectColumn::new("e\u{301}"),
                    ]),
            ],
            SelectStyles {
                prefix: dim,
                row_marker: dim,
                secondary: dim,
                label: bright,
                selected_bg: band,
                ..SelectStyles::default()
            },
        );
        let rows = row_cells(&mut select, 32, 6);
        assert_eq!(rows.len(), 2);
        // Read terminal cells, not byte offsets: the wide grapheme has a
        // continuation cell and the combining sequence occupies one cell.
        assert_eq!(rows[0][2].char.grapheme(), "界");
        assert_eq!(rows[0][2].char.width, 2);
        assert_eq!(rows[0][4].char.grapheme(), "x");
        assert_eq!(row_text(&rows[0][5..19]), "  1h     123  ");
        assert_eq!(row_text(&rows[1][..19]), "  tag  20min    4  ");
        assert_eq!(rows[0][19].char.grapheme(), "界");
        assert_eq!(rows[1][19].char.grapheme(), "e\u{301}");
        assert_eq!(rows[1][20].char.grapheme(), " ");
        for row in &rows {
            assert_eq!(row_text(&row[23..]), "preview …");
        }
        for (index, row) in rows.iter().enumerate() {
            // RichText stores a wide glyph's style on its leading cell.
            let mut col = 0;
            while col < 23 {
                assert!(row[col].style.dim);
                assert_eq!(row[col].style.fg, dim.fg);
                col += usize::from(row[col].char.width).max(1);
            }
            assert!(
                row[23..30]
                    .iter()
                    .all(|cell| { cell.style.fg == bright.fg && !cell.style.dim })
            );
            assert!(
                row.iter().all(|cell| {
                    cell.style.bg == if index == 0 { band } else { Color::Default }
                })
            );
        }

        // Hiding the only marked item must not move the surviving row left.
        type_str(&mut select, "bravo");
        let filtered = row_cells(&mut select, 32, 6);
        assert_eq!(filtered.len(), 1);
        assert_eq!(row_text(&filtered[0]), row_text(&rows[1]));

        // A hidden batch still widens shared columns. Missing fields occupy
        // their slots, while the entirely empty second column adds no gap.
        select.extend_items(vec![
            SelectItem::new("other", "other").with_columns(vec![SelectColumn::new("longer")]),
        ]);
        let extended = row_cells(&mut select, 40, 6);
        assert_eq!(extended[0][17].char.grapheme(), "4");
        assert_eq!(extended[0][24].char.grapheme(), "p");

        select.set_items(vec![
            SelectItem::new("preview", "bravo").with_columns(vec![
                SelectColumn::new(""),
                SelectColumn::new(""),
                SelectColumn::new("7").with_alignment(TextAlign::Right),
            ]),
            SelectItem::new("hidden", "other").with_columns(vec![SelectColumn::new("abc")]),
        ]);
        let replaced = row_cells(&mut select, 24, 6);
        assert_eq!(row_text(&replaced[0]).trim_end(), "     7  preview");
    }

    /// The prefix column is right-aligned within the widest prefix and drawn
    /// in the prefix style.
    #[test]
    fn prefix_is_right_aligned_and_dim() {
        let dim = Style {
            fg: Color::Index(8),
            dim: true,
            ..Style::default()
        };
        let mut select = FilterableSelect::new(
            vec![
                SelectItem::new("alpha", "alpha").with_prefix("Long"),
                SelectItem::new("bravo", "bravo").with_prefix("X"),
            ],
            SelectStyles {
                prefix: dim,
                ..SelectStyles::default()
            },
        );
        // Read the second (non-cursored) row so no band tint colors the cells.
        let rows = row_cells(&mut select, 40, 10);
        let bravo = &rows[1];
        // "X" is right-aligned within the 4-wide column, so three pad spaces
        // precede it and the label starts after the two-cell gap.
        assert_eq!(&row_text(bravo)[..8], "   X  br");
        assert_eq!(bravo[3].char.grapheme(), "X");
        assert_eq!(bravo[3].style, dim, "prefix cell carries the prefix style");
        // The right-alignment padding shares the prefix style.
        assert!(bravo[..3].iter().all(|c| c.style == dim));
    }

    /// The shortcut in the right slot is drawn in the shortcut style, distinct
    /// from the label style.
    #[test]
    fn shortcut_uses_the_shortcut_style_not_the_label_style() {
        let label_style = Style {
            fg: Color::Index(1),
            bold: true,
            ..Style::default()
        };
        let shortcut_style = Style {
            fg: Color::Index(4),
            bold: true,
            ..Style::default()
        };
        let mut select = FilterableSelect::new(
            vec![
                SelectItem::new("run", "run")
                    .with_prefix("Cat")
                    .with_shortcut("Ctrl+R"),
                SelectItem::new("stop", "stop")
                    .with_prefix("Cat")
                    .with_shortcut("Ctrl+S"),
            ],
            SelectStyles {
                label: label_style,
                shortcut: shortcut_style,
                ..SelectStyles::default()
            },
        );
        let rows = row_cells(&mut select, 40, 10);
        // Second row, so no band tint on the cells.
        let stop = &rows[1];
        let text = row_text(stop);
        let short_at = text.find("Ctrl+S").expect("shortcut rendered");
        let label_at = text.find("stop").expect("label rendered");
        assert_eq!(
            stop[short_at].style, shortcut_style,
            "shortcut cell uses the shortcut style"
        );
        assert_eq!(
            stop[label_at].style, label_style,
            "label cell uses the label style"
        );
        assert_ne!(
            shortcut_style.fg, label_style.fg,
            "the two styles are actually distinct"
        );
    }

    /// The label column is sized from the widest label across all items, so
    /// the shortcut column stays put as the filter narrows the visible set.
    #[test]
    fn label_column_width_is_stable_under_filtering() {
        let mut select = FilterableSelect::new(
            vec![
                SelectItem::new("short", "short")
                    .with_prefix("P")
                    .with_shortcut("A"),
                SelectItem::new("muchlongerlabel", "muchlongerlabel")
                    .with_prefix("P")
                    .with_shortcut("B"),
            ],
            SelectStyles::default(),
        );
        // prefix column "P" (1) + gap (2) + label padded to 15 + 2 = 17.
        // The shortcut therefore starts at column 3 + 17 = 20.
        let rows = row_cells(&mut select, 40, 10);
        assert_eq!(
            row_text(&rows[0]).find('A'),
            Some(20),
            "shortcut aligned to the widest label's column"
        );

        // Filtering down to only the short-label row must not pull the column
        // in: the width comes from the full item set, not the visible subset.
        for c in "short".chars() {
            send(&mut select, &typed(c));
        }
        assert_eq!(select.visible_labels(), ["short"]);
        let rows = row_cells(&mut select, 40, 10);
        assert_eq!(
            row_text(&rows[0]).find('A'),
            Some(20),
            "the label column did not shift while filtering"
        );
    }

    /// A shortcut wins the right slot over a description, and takes the
    /// shortcut style.
    #[test]
    fn shortcut_wins_over_description_in_the_right_slot() {
        let shortcut_style = Style {
            fg: Color::Index(4),
            bold: true,
            ..Style::default()
        };
        let secondary = Style {
            fg: Color::Index(5),
            ..Style::default()
        };
        let mut select = FilterableSelect::new(
            vec![
                SelectItem::new("cmd", "cmd")
                    .with_prefix("C")
                    .with_shortcut("Ctrl+X")
                    .with_description("should not show"),
                SelectItem::new("other", "other")
                    .with_prefix("C")
                    .with_shortcut("Ctrl+Y"),
            ],
            SelectStyles {
                shortcut: shortcut_style,
                secondary,
                ..SelectStyles::default()
            },
        );
        let rows = row_cells(&mut select, 40, 10);
        let text = row_text(&rows[0]);
        assert!(text.contains("Ctrl+X"), "shortcut rendered: {text:?}");
        assert!(
            !text.contains("should not show"),
            "description suppressed when a shortcut is set: {text:?}"
        );
        let at = text.find("Ctrl+X").expect("shortcut rendered");
        assert_eq!(
            rows[0][at].style.fg, shortcut_style.fg,
            "the right slot carries the shortcut style"
        );
    }

    /// The opt-in vertical scroll bar reserves the rightmost column and
    /// draws a thumb once the list overflows the viewport; while it is off
    /// the list keeps the full width and no thumb column appears.
    #[test]
    fn scrollbar_reserves_a_column_and_shows_a_thumb_on_overflow() {
        let labels: Vec<String> = (0..20).map(|i| format!("row{i}")).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut select = FilterableSelect::new(items(&refs), SelectStyles::default());

        // Off by default: the bars draw the list at full width, no thumb.
        let surface = select.draw(&draw_ctx(30, 10));
        let bars = &surface.children[1].surface;
        assert_eq!(
            bars.children.len(),
            1,
            "no thumb column while the bar is off"
        );
        assert_eq!(
            bars.children[0].surface.size.width, 30,
            "list spans the full width with the bar off"
        );

        // On: 20 rows overflow the 8-row list viewport, so the list narrows
        // by one column and a one-column thumb bar is drawn on the right.
        select.set_show_scrollbar(true);
        let surface = select.draw(&draw_ctx(30, 10));
        let bars = &surface.children[1].surface;
        assert_eq!(
            bars.children[0].surface.size.width, 29,
            "list left a column for the bar"
        );
        let thumb = bars
            .children
            .iter()
            .find(|c| c.origin.col == 29)
            .expect("thumb bar drawn on the rightmost column");
        assert_eq!(thumb.surface.size.width, 1, "thumb bar is one column wide");
    }

    /// With the bar on but the list fitting the viewport, no bar is drawn and
    /// the list keeps the full width. The column is reclaimed until the list
    /// actually overflows.
    #[test]
    fn scrollbar_hidden_while_the_list_fits() {
        let mut select = FilterableSelect::new(items(&["a0", "a1", "a2"]), SelectStyles::default());
        select.set_show_scrollbar(true);
        // Three rows in an 8-row list viewport: nothing overflows.
        let surface = select.draw(&draw_ctx(30, 10));
        let bars = &surface.children[1].surface;
        assert_eq!(
            bars.children.len(),
            1,
            "no thumb column while the list fits, only the inner view"
        );
        assert_eq!(
            bars.children[0].surface.size.width, 30,
            "the list keeps the full width until it overflows"
        );
    }

    /// Pre-selecting the active row moves the cursor (and thus the band)
    /// onto it on open.
    #[test]
    fn select_matching_preselects_a_row() {
        let select = sample();
        select.select_matching(|item| item.filter_key == "charlie");
        assert_eq!(select.selected().map(|i| i.label), Some("charlie".into()));
        // A no-match predicate leaves the cursor where it was.
        select.select_matching(|item| item.filter_key == "nope");
        assert_eq!(select.selected().map(|i| i.label), Some("charlie".into()));
    }

    #[test]
    fn ranked_snapshots_follow_the_top_until_interaction() {
        let mut select = FilterableSelect::new(items(&["old"]), SelectStyles::default());
        select.set_ranked_items(items(&["new", "old"]));
        assert_eq!(select.selected().unwrap().filter_key, "new");
        assert_eq!(select.list.borrow().scroll_top(), 0);
        // An explicit choice to remain at the top still stops following.
        send(&mut select, &key(Key::UP, Modifiers::empty()));
        select.set_ranked_items(items(&["newest", "new", "old"]));
        assert_eq!(select.selected().unwrap().filter_key, "new");
        send(&mut select, &typed('o'));
        assert_eq!(select.selected().unwrap().filter_key, "old");
        select.set_ranked_items(items(&["o", "old"]));
        assert_eq!(select.selected().unwrap().filter_key, "old");
        select.set_ranked_items(items(&["other"]));
        assert_eq!(select.selected().unwrap().filter_key, "other");
        select.set_ranked_items(vec![]);
        assert!(select.selected().is_none());
        select.set_ranked_items(items(&["only"]));
        assert_eq!(select.selected().unwrap().filter_key, "only");
    }

    #[test]
    fn query_edits_stop_following_even_without_navigation_and_after_clearing() {
        let mut select = FilterableSelect::new(items(&["old"]), SelectStyles::default());
        send(&mut select, &typed('o'));
        select.set_ranked_items(items(&["other", "old"]));
        assert_eq!(select.selected().unwrap().filter_key, "old");
        send(&mut select, &key(Key::BACKSPACE, Modifiers::empty()));
        assert!(select.query().is_empty());
        select.set_ranked_items(items(&["newest", "other", "old"]));
        assert_eq!(select.selected().unwrap().filter_key, "other");

        // Replacing the source starts fresh, unlike another background snapshot.
        select.set_items(items(&["old"]));
        select.set_ranked_items(items(&["new", "old"]));
        assert_eq!(select.selected().unwrap().filter_key, "new");
        select.select_matching(|item| item.filter_key == "old");
        select.set_ranked_items(items(&["newest", "new", "old"]));
        assert_eq!(select.selected().unwrap().filter_key, "old");
    }

    #[test]
    fn scrolling_without_selection_stops_following_and_keeps_queued_motion() {
        let mut select = FilterableSelect::new(
            items(&["a", "b", "c", "d", "e", "f"]),
            SelectStyles::default(),
        );
        let ctx = draw_ctx(30, 5);
        select.draw(&ctx);
        let wheel = Event::Mouse(crate::mouse::Mouse {
            col: 3,
            row: 3,
            xoffset: 0,
            yoffset: 0,
            button: crate::mouse::Button::WheelDown,
            mods: crate::mouse::Modifiers::default(),
            kind: crate::mouse::Type::Press,
        });
        let mut event_ctx = EventContext::new();
        select.capture_event(&mut event_ctx, &wheel);
        select
            .list
            .borrow_mut()
            .handle_event(&mut event_ctx, &wheel);
        // The snapshot arrives before the queued wheel movement is drawn.
        select.set_ranked_items(items(&["new", "a", "b", "c", "d", "e", "f"]));
        select.draw(&ctx);
        assert_eq!(select.selected().unwrap().filter_key, "a");
        let top = usize::try_from(select.list.borrow().scroll_top()).unwrap();
        assert_eq!(select.visible_labels()[top], "b");
    }

    #[test]
    fn ranked_snapshots_anchor_the_drawn_selection_or_offscreen_reading_position() {
        let rows = || {
            (0..60)
                .map(|i| SelectItem::new(format!("row{i:02}"), format!("row{i:02}")))
                .collect()
        };
        let mut select = FilterableSelect::new(rows(), SelectStyles::default());
        let ctx = draw_ctx(30, 10);
        select.draw(&ctx);
        for _ in 0..20 {
            send(&mut select, &key(Key::DOWN, Modifiers::empty()));
            select.draw(&ctx);
        }
        let screen_row = select.list.borrow().cursor - select.list.borrow().scroll_top();
        assert!(screen_row > 0, "fixture has the selection below the top");
        let mut incoming = items(&["new0", "new1"]);
        incoming.extend(rows());
        select.set_ranked_items(incoming);
        select.draw(&ctx);
        assert_eq!(select.selected().unwrap().filter_key, "row20");
        assert_eq!(
            select.list.borrow().cursor - select.list.borrow().scroll_top(),
            screen_row
        );

        // Wheel input can leave the keyboard selection outside the viewport.
        select.list.borrow_mut().scroll_lines(15);
        select.draw(&ctx);
        let top = select.visible_labels()
            [usize::try_from(select.list.borrow().scroll_top()).expect("top fits usize")]
        .clone();
        assert!(select.list.borrow().cursor < select.list.borrow().scroll_top());
        // Another wheel event is queued but has not drawn when the batch lands.
        select.list.borrow_mut().scroll_lines(2);
        let mut incoming = items(&["newest", "new0", "new1"]);
        incoming.extend(rows());
        select.set_ranked_items(incoming);
        select.draw(&ctx);
        let new_top = usize::try_from(select.list.borrow().scroll_top()).expect("top fits usize");
        assert_eq!(select.visible_labels()[new_top - 2], top);
        assert_eq!(select.selected().unwrap().filter_key, "row20");

        // Eviction clamps both anchors, and an identical snapshot does not
        // consume a navigation event still waiting for a draw.
        select.set_ranked_items(items(&["last0", "last1"]));
        select.draw(&ctx);
        assert_eq!(select.selected().unwrap().filter_key, "last1");
        send(&mut select, &key(Key::UP, Modifiers::empty()));
        select.set_ranked_items(items(&["last0", "last1"]));
        select.draw(&ctx);
        assert_eq!(select.selected().unwrap().filter_key, "last0");
    }

    // --- Parity between the incremental paths and a full rescore. ---
    //
    // The optimization only holds if the incremental `visible` order stays
    // byte-identical to scoring the whole set from scratch. These tests pin
    // that down against an independent full-rescore oracle.

    fn items(keys: &[&str]) -> Vec<SelectItem> {
        keys.iter().map(|k| SelectItem::new(*k, *k)).collect()
    }

    /// The private `visible` indices (score dropped), for direct comparison.
    fn visible_indices(select: &FilterableSelect) -> Vec<usize> {
        select
            .state
            .borrow()
            .visible
            .iter()
            .map(|&(i, _)| i)
            .collect()
    }

    /// The full-rescore oracle: score every item from scratch at `query`,
    /// exactly as `full_filter` does. Independent of the incremental paths.
    fn full_rescore_indices(items: &[SelectItem], query: &str) -> Vec<usize> {
        let mut matcher = FuzzyMatcher::new();
        matcher
            .filter_scored(items.iter().enumerate(), query, |(_, item)| {
                item.filter_key.as_str()
            })
            .into_iter()
            .map(|((i, _), _)| i)
            .collect()
    }

    fn type_str(select: &mut FilterableSelect, s: &str) {
        for c in s.chars() {
            send(select, &typed(c));
        }
    }

    /// One step in a keystroke script: append a char or delete the last one.
    #[derive(Clone, Copy)]
    enum Step {
        Type(char),
        Backspace,
    }

    /// Drive `keys` one step at a time through the real `on_change` path and,
    /// after every step, assert the incremental `visible` equals a full
    /// rescore at the same query. Typing exercises `narrow_filter` (append),
    /// backspace exercises `full_filter` (non-append).
    fn assert_incremental_matches_full(item_keys: &[&str], keys: &[Step]) {
        let items = items(item_keys);
        let mut select = FilterableSelect::new(items.clone(), SelectStyles::default());
        let mut query = String::new();
        assert_eq!(
            visible_indices(&select),
            full_rescore_indices(&items, &query)
        );
        for step in keys {
            match step {
                Step::Type(c) => {
                    send(&mut select, &typed(*c));
                    query.push(*c);
                }
                Step::Backspace => {
                    send(&mut select, &key(Key::BACKSPACE, Modifiers::empty()));
                    query.pop();
                }
            }
            assert_eq!(
                visible_indices(&select),
                full_rescore_indices(&items, &query),
                "visible diverged from full rescore at query {query:?}"
            );
            // Every filter change resets the cursor to the top.
            assert_eq!(
                select.list.borrow().cursor,
                0,
                "cursor not reset to top at query {query:?}"
            );
        }
    }

    #[test]
    fn narrow_path_matches_full_rescore_multi_token() {
        // Multi-token filter keys plus a duplicate key (items 1 and 4) that
        // ties on score and must keep original order.
        let keys = &[
            "openai gpt-5.5",
            "openai gpt-5.1",
            "anthropic claude",
            "openai o3",
            "openai gpt-5.1",
        ];
        // Appends only, including the space that opens a second token.
        let script: Vec<Step> = "openai 5".chars().map(Step::Type).collect();
        assert_incremental_matches_full(keys, &script);
        // Different token first, then narrow.
        let script: Vec<Step> = "anthro cl".chars().map(Step::Type).collect();
        assert_incremental_matches_full(keys, &script);
    }

    #[test]
    fn narrow_path_matches_full_rescore_exact_bonus() {
        // "cl" is an exact match and must outrank the longer partials.
        let keys = &["cl", "clone", "close", "clang"];
        let script: Vec<Step> = "clos".chars().map(Step::Type).collect();
        assert_incremental_matches_full(keys, &script);
    }

    #[test]
    fn narrow_path_matches_full_rescore_ties() {
        // Three identical keys tie on score; a fourth scores differently.
        let keys = &["aa", "aa", "aa", "abracadabra"];
        let script: Vec<Step> = "aaa".chars().map(Step::Type).collect();
        assert_incremental_matches_full(keys, &script);
    }

    #[test]
    fn full_path_matches_on_backspace_and_edits() {
        // Interleave appends and backspaces so both the narrow and full
        // branches of `on_change` are exercised against the oracle.
        let keys = &["cl", "clone", "close", "clang", "abracadabra", "cl"];
        use Step::{Backspace as B, Type as T};
        let script = [
            T('c'),
            T('l'),
            T('o'),
            B,
            B,
            T('a'),
            B,
            T('l'),
            T('o'),
            T('s'),
            B,
            B,
            B,
        ];
        assert_incremental_matches_full(keys, &script);
    }

    /// Build a full-rescore reference: an empty select, the query typed in,
    /// then `set_items` of the whole set (a single full rescore at `query`).
    fn full_rescore_select(item_keys: &[&str], query: &str) -> FilterableSelect {
        let mut select = FilterableSelect::new(Vec::new(), SelectStyles::default());
        type_str(&mut select, query);
        select.set_items(items(item_keys));
        select
    }

    /// Stream `item_keys` through `extend_items` in `batch_sizes` batches (at
    /// the fixed `query`) and assert the final `visible` and `item_count`
    /// match a single full rescore of the whole set at that query.
    fn assert_extend_matches_full(item_keys: &[&str], query: &str, batch_sizes: &[usize]) {
        assert_eq!(
            batch_sizes.iter().sum::<usize>(),
            item_keys.len(),
            "batch sizes must cover the item set"
        );
        let mut select = FilterableSelect::new(Vec::new(), SelectStyles::default());
        type_str(&mut select, query);
        let mut start = 0;
        for &size in batch_sizes {
            let batch = items(&item_keys[start..start + size]);
            select.extend_items(batch);
            start += size;
        }

        let reference = full_rescore_select(item_keys, query);
        assert_eq!(
            visible_indices(&select),
            visible_indices(&reference),
            "streamed visible diverged from full rescore at query {query:?}"
        );
        assert_eq!(
            select.list.borrow().item_count,
            reference.list.borrow().item_count,
            "streamed item_count diverged at query {query:?}"
        );
    }

    #[test]
    fn incremental_extend_matches_full_rescore() {
        // Scores vary and exact matches (indices 0 and 6) tie, so the merge
        // must interleave a late high-scoring batch ahead of earlier rows.
        let keys = &["cl", "clone", "xcl", "close", "clang", "recall", "cl"];
        // Non-empty query, several batch splittings.
        for batches in [
            vec![7],
            vec![3, 2, 2],
            vec![1, 1, 1, 1, 1, 1, 1],
            vec![6, 1],
            vec![1, 6],
        ] {
            assert_extend_matches_full(keys, "cl", &batches);
        }
        // Empty query: extend appends in index order.
        for batches in [vec![3, 2, 2], vec![1, 1, 1, 1, 1, 1, 1]] {
            assert_extend_matches_full(keys, "", &batches);
        }
    }

    #[test]
    fn on_change_resets_cursor_to_top() {
        let mut select = sample();
        send(&mut select, &key(Key::DOWN, Modifiers::empty()));
        assert_eq!(select.list.borrow().cursor, 1);
        // Typing narrows and pulls the cursor back to the first row.
        send(&mut select, &typed('a'));
        assert_eq!(select.list.borrow().cursor, 0);
    }

    #[test]
    fn extend_items_preserves_the_cursor() {
        // No query, so all rows stay visible and the cursor index is stable.
        let mut select =
            FilterableSelect::new(items(&["a0", "a1", "a2", "a3"]), SelectStyles::default());
        send(&mut select, &key(Key::DOWN, Modifiers::empty()));
        send(&mut select, &key(Key::DOWN, Modifiers::empty()));
        assert_eq!(select.list.borrow().cursor, 2);
        select.extend_items(items(&["a4", "a5"]));
        assert_eq!(
            select.list.borrow().cursor,
            2,
            "streamed append kept the highlight in place"
        );
        assert_eq!(select.list.borrow().item_count, Some(6));
    }

    /// A streamed append must not re-anchor the scroll onto the cursor row.
    /// Moving the cursor down leaves the scroll `top` where it was
    /// (`ensure_scroll` defers the reveal to draw via `wants_cursor`), so an
    /// append that re-pinned `top` to the cursor would yank the viewport.
    #[test]
    fn extend_items_preserves_the_scroll_anchor() {
        let mut select =
            FilterableSelect::new(items(&["a0", "a1", "a2", "a3"]), SelectStyles::default());
        send(&mut select, &key(Key::DOWN, Modifiers::empty()));
        send(&mut select, &key(Key::DOWN, Modifiers::empty()));
        assert_eq!(select.list.borrow().cursor, 2);
        assert_eq!(
            select.list.borrow().scroll_top(),
            0,
            "moving the cursor down leaves the anchor at the top"
        );
        select.extend_items(items(&["a4", "a5"]));
        assert_eq!(
            select.list.borrow().scroll_top(),
            0,
            "append kept the scroll anchor instead of re-pinning it to the cursor"
        );
    }

    /// Replacing a row in place keeps the cursor on its row and leaves the
    /// scroll anchor alone, whether the replaced row is the cursored one, above
    /// it, or below it. Under a query the cursor follows its row when the new
    /// text changes the ranking.
    #[test]
    fn update_item_keeps_the_cursor_row_and_scroll_anchor() {
        let keys: Vec<String> = (0..40).map(|i| format!("row{i:02}")).collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let select = FilterableSelect::new(items(&refs), SelectStyles::default());
        select.list.borrow_mut().jump_to_item(30);
        select.list.borrow_mut().cursor = 33;
        for index in [33usize, 5, 31] {
            select.update_item(index, SelectItem::new(format!("filled {index}"), "zzz"));
            assert_eq!(
                select.list.borrow().cursor,
                33,
                "update of {index} moved the cursor"
            );
            assert_eq!(
                select.list.borrow().scroll_top(),
                30,
                "update of {index} moved the anchor"
            );
        }
        assert_eq!(select.visible_labels()[5], "filled 5");
        assert_eq!(select.selected().unwrap().label, "filled 33");

        // A query is live and the cursored row drops out of the match: the
        // cursor stays in range rather than pointing past the end.
        let select = FilterableSelect::new(items(&["ab", "ab", "ab"]), SelectStyles::default());
        {
            let mut state = select.state.borrow_mut();
            state.query = "ab".to_string();
            full_filter(&mut state, &mut select.list.borrow_mut());
        }
        select.list.borrow_mut().cursor = 2;
        select.update_item(2, SelectItem::new("gone", "xy"));
        assert_eq!(select.visible_labels().len(), 2);
        assert_eq!(select.list.borrow().cursor, 1);
        // The cursored row itself changes rank: the cursor follows it.
        select.list.borrow_mut().cursor = 0;
        select.update_item(0, SelectItem::new("weak", "a_b"));
        assert_eq!(select.selected().unwrap().label, "weak");
        assert_eq!(select.list.borrow().cursor, 1);
    }

    /// `narrow_filter` sorts its candidates by original index before rescoring
    /// so its output does not depend on the order the current `visible` holds
    /// them in. This locks that invariant down directly: a rank-order `visible`
    /// never reverses tied items in practice, but the sort makes the result
    /// order-independent, which is what keeps a narrow byte-identical to a full
    /// rescore over any surviving set (including future scoring changes).
    #[test]
    fn narrow_filter_output_is_independent_of_visible_order() {
        // These four keys all tie at "ab" (equal score), so a full rescore
        // keeps them in index order.
        let keys = ["abc", "ab_c", "abcz", "abcabc"];
        let item_vec = items(&keys);
        let select = FilterableSelect::new(item_vec.clone(), SelectStyles::default());
        {
            let mut state = select.state.borrow_mut();
            let mut list = select.list.borrow_mut();
            state.query = "ab".to_string();
            full_filter(&mut state, &mut list);
            assert_eq!(
                visible_indices_of(&state),
                vec![0, 1, 2, 3],
                "sanity: the tied keys start in index order"
            );
            // Scramble the ranked view, then narrow at the same query. The
            // candidate sort must restore index order for the tie.
            state.visible.reverse();
            narrow_filter(&mut state, &mut list);
        }
        assert_eq!(
            visible_indices(&select),
            full_rescore_indices(&item_vec, "ab")
        );
        assert_eq!(visible_indices(&select), vec![0, 1, 2, 3]);
    }

    // --- The optional query scope. ---

    /// Three rows whose scope keys are a strict subset of their filter keys,
    /// plus one row with no scope key at all whose filter key carries a
    /// literal `#`.
    fn scoped_items() -> Vec<SelectItem> {
        vec![
            SelectItem::new("alpha", "alpha fix-auth").with_scope_key("fix-auth"),
            SelectItem::new("bravo", "bravo fixture").with_scope_key("fixture"),
            SelectItem::new("charlie", "charlie fix the prompt"),
            SelectItem::new("delta", "delta issue #42"),
        ]
    }

    /// A select over [`scoped_items`] with `#` as its scope sigil, with
    /// `query` typed in one key at a time through the real filter field.
    fn scoped_select(query: &str) -> FilterableSelect {
        let mut select = FilterableSelect::new(scoped_items(), SelectStyles::default());
        select.set_scope_sigil('#');
        type_str(&mut select, query);
        select
    }

    /// The sigil narrows the query to the scope keys: an unscoped query sees
    /// the whole corpus, a scoped one sees only rows that have a scope key,
    /// and only their key.
    #[test]
    fn the_scope_sigil_restricts_matching_to_the_scope_key() {
        assert_eq!(
            scoped_select("fix").visible_labels(),
            ["alpha", "bravo", "charlie"],
            "unscoped, the corpus answers"
        );
        assert_eq!(
            scoped_select("#fix").visible_labels(),
            ["alpha", "bravo"],
            "scoped, the row whose match was in the prompt drops out"
        );
        assert_eq!(
            scoped_select("#auth").visible_labels(),
            ["alpha"],
            "and the match has to be in the key, not the label"
        );
        assert_eq!(
            scoped_select("#alpha").visible_labels(),
            Vec::<String>::new(),
            "a row that has a key is still matched only on that key",
        );
    }

    /// The scope folds case exactly the way the corpus does, which is the
    /// matcher's rule and not this widget's: the text is folded, the query is
    /// taken as typed, so a lowercase query finds a label of any case.
    #[test]
    fn a_scoped_query_folds_case_the_way_the_corpus_does() {
        let item = SelectItem::new("echo", "echo Fix-Auth").with_scope_key("Fix-Auth");
        let mut select = FilterableSelect::new(vec![item], SelectStyles::default());
        select.set_scope_sigil('#');
        type_str(&mut select, "fix");
        assert_eq!(select.visible_labels(), ["echo"], "unscoped");
        for _ in 0..3 {
            send(&mut select, &key(Key::BACKSPACE, Modifiers::empty()));
        }
        type_str(&mut select, "#fix");
        assert_eq!(select.visible_labels(), ["echo"], "and scoped");
    }

    /// The bare sigil is the empty scoped query, which every scope key
    /// matches and no missing one does, so it lists exactly the rows that
    /// have a key.
    #[test]
    fn a_bare_scope_sigil_lists_the_rows_that_have_a_key() {
        assert_eq!(scoped_select("#").visible_labels(), ["alpha", "bravo"]);
    }

    /// Nothing at all carries a scope key: the sigil then matches nothing
    /// rather than falling back to the corpus.
    #[test]
    fn a_scoped_query_over_rows_without_keys_matches_nothing() {
        let mut select = FilterableSelect::new(items(&["alpha", "bravo"]), SelectStyles::default());
        select.set_scope_sigil('#');
        type_str(&mut select, "#");
        assert!(select.visible_labels().is_empty(), "the bare sigil");
        type_str(&mut select, "al");
        assert!(select.visible_labels().is_empty(), "and a scoped query");
    }

    /// The sigil is only a sigil at the front of the query. Elsewhere it is
    /// ordinary text matched against the corpus, so a row with no scope key
    /// at all is still findable by a `#` in its filter key.
    #[test]
    fn the_scope_sigil_is_literal_anywhere_but_the_front() {
        assert_eq!(
            scoped_select("issue #42").visible_labels(),
            ["delta"],
            "a mid-query # matched the corpus literally"
        );
    }

    /// Without a sigil configured the character is plain text, so a select
    /// that never opted in keeps matching exactly as it did.
    #[test]
    fn without_a_sigil_the_character_is_plain_text() {
        let mut select = FilterableSelect::new(scoped_items(), SelectStyles::default());
        type_str(&mut select, "#4");
        assert_eq!(select.visible_labels(), ["delta"]);
    }

    /// Editing across the scope boundary keeps the incremental paths honest:
    /// typing the sigil narrows (append) and deleting it rescores in full,
    /// and both land on the set a fresh select at that query would show.
    #[test]
    fn editing_across_the_scope_boundary_matches_a_fresh_filter() {
        let mut select = scoped_select("#fix");
        assert_eq!(select.visible_labels(), ["alpha", "bravo"]);
        for _ in 0..3 {
            send(&mut select, &key(Key::BACKSPACE, Modifiers::empty()));
        }
        assert_eq!(select.visible_labels(), scoped_select("#").visible_labels());
        send(&mut select, &key(Key::BACKSPACE, Modifiers::empty()));
        assert_eq!(
            select.visible_labels(),
            ["alpha", "bravo", "charlie", "delta"],
            "deleting the sigil restored the whole corpus"
        );
    }

    /// A streamed batch is ranked under the live scope, so rows arriving
    /// while a scoped query is up are filtered by it like the rest.
    #[test]
    fn a_batch_streamed_under_a_scoped_query_is_scoped_too() {
        let mut select = FilterableSelect::new(Vec::new(), SelectStyles::default());
        select.set_scope_sigil('#');
        type_str(&mut select, "#fix");
        select.extend_items(scoped_items());
        assert_eq!(select.visible_labels(), ["alpha", "bravo"]);
    }
}

/// Helper shared by tests that hold a `SelectState` borrow directly.
#[cfg(test)]
fn visible_indices_of(state: &SelectState) -> Vec<usize> {
    state.visible.iter().map(|&(i, _)| i).collect()
}
