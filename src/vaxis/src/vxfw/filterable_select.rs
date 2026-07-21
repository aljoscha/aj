//! [`FilterableSelect`]: a fuzzy-filtered pick list.
//!
//! Most selector overlays are the same shape: a one-line filter input above a
//! navigable result list, where typing filters and re-ranks the rows, the
//! arrow keys move the highlight, Enter confirms the highlighted row, and
//! Escape cancels. This widget owns that mechanics, composing a [`TextField`]
//! filter over a [`ListView`], with a [`FuzzyMatcher`] ranking the rows as the
//! filter text changes.
//!
//! # Focus and key routing
//!
//! Focus belongs on the filter [`TextField`] (see
//! [`focus_target`](FilterableSelect::focus_target)) so its cursor renders
//! and printable keys edit the query. The select widget is the field's
//! ancestor on the focus path, so it intercepts the selector chords in its
//! capturing phase before the field sees them: Escape cancels, Enter (or
//! Ctrl+J) confirms, and Up/Down/Ctrl+P/Ctrl+N are forwarded to the list's
//! cursor. Everything else falls through to the field at-target.
//!
//! # Selection band (Spec E, decision E-7)
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
use std::rc::Rc;

use crate::cell::{Cell, Character, Color, Style};
use crate::fuzzy::FuzzyMatcher;
use crate::key::{Key, Modifiers};
use crate::vxfw::{
    Builder, DrawContext, Event, EventContext, ListView, MaxSize, PromptInput, RelativePoint,
    RichText, ScrollBars, Size, Source, SubSurface, Surface, TextField, TextSpan, Widget,
    WidgetRef, WidthBasis, draw_widget, to_widget_ref,
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
            shortcut: Style::default(),
            secondary: Style::default(),
            scrollbar_thumb: Style::default(),
            marker: Style::default(),
        }
    }
}

/// One selectable row: what the list shows and what the filter matches.
///
/// The `label` and `filter_key` are separate on purpose: the display label is
/// the human title while the filter should match a curated key (a category
/// plus a title, say) rather than the rendered columns. The widget lays out
/// the columns itself, so the label carries only the title, never padding.
///
/// A row can carry three optional columns around the label: a `prefix`
/// (right-aligned metadata column, e.g. a command category), a `shortcut` (a
/// key hint), and a `description`. The shortcut and the description share the
/// right slot, and a shortcut wins when both are set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem {
    /// Row text shown in the list.
    pub label: String,
    /// Text the fuzzy filter matches and ranks against.
    pub filter_key: String,
    /// Optional right-aligned metadata column drawn to the left of the label
    /// (a command category, say), styled with [`SelectStyles::prefix`].
    pub prefix: Option<String>,
    /// Optional key hint drawn in the right slot in
    /// [`SelectStyles::shortcut`]. Wins the slot over `description`.
    pub shortcut: Option<String>,
    /// Optional dim secondary column (a wire-level id, a one-line
    /// description), rendered in the right slot when no `shortcut` is set.
    pub description: Option<String>,
}

impl SelectItem {
    /// Builds an item from its display label and filter key, with no extra
    /// columns.
    pub fn new(label: impl Into<String>, filter_key: impl Into<String>) -> SelectItem {
        SelectItem {
            label: label.into(),
            filter_key: filter_key.into(),
            prefix: None,
            shortcut: None,
            description: None,
        }
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

    /// Adds a dim secondary column shown in the right slot after the label.
    pub fn with_description(mut self, description: impl Into<String>) -> SelectItem {
        self.description = Some(description.into());
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
    /// The current filter text, mirrored from the `TextField` on change.
    query: String,
    matcher: FuzzyMatcher,
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
}

/// Gap between the right-aligned prefix column and the label.
const PREFIX_COLUMN_GAP: usize = 2;
/// Padding added past the widest label, so a shortcut in the right slot sits
/// clear of the longest label rather than flush against it.
const LABEL_COLUMN_PADDING: usize = 2;

/// Recomputes the prefix and label column widths from the full item set.
///
/// Called wherever `items` changes (construction, `set_items`, `extend_items`)
/// so the columns stay sized to the widest content even as batches stream in.
///
/// NOTE: widths count chars, not grapheme display width. Padding and width use
/// the same measure so it is internally consistent, but a wide or combining
/// char in a prefix or a shortcut-bearing label would misalign the shortcut
/// column against the `RichText` layout. Every caller today is ASCII. Switch to
/// gwidth here and in `build_row` if a non-ASCII prefix/shortcut caller appears.
fn recompute_widths(state: &mut SelectState) {
    state.prefix_width = state
        .items
        .iter()
        .filter_map(|item| item.prefix.as_deref())
        .map(|prefix| prefix.chars().count())
        .max()
        .unwrap_or(0);
    state.label_width = state
        .items
        .iter()
        .map(|item| item.label.chars().count())
        .max()
        .unwrap_or(0)
        + LABEL_COLUMN_PADDING;
}

/// Builds one banded row widget per visible index, restyling the row the
/// cursor is on. The `state` and `styles` cells are shared with the widget, so
/// a filter change or a restyle is picked up without rebuilding the builder.
struct RowBuilder {
    state: Rc<RefCell<SelectState>>,
    styles: Rc<RefCell<SelectStyles>>,
}

impl Builder for RowBuilder {
    fn item_at_idx(&self, idx: usize, cursor: usize) -> Option<WidgetRef> {
        let state = self.state.borrow();
        let &(item_idx, _) = state.visible.get(idx)?;
        let styles = self.styles.borrow();
        Some(build_row(
            &state.items[item_idx],
            idx == cursor,
            &styles,
            state.prefix_width,
            state.label_width,
        ))
    }
}

/// Build one row: a full-width [`RichText`] whose cells all carry
/// `selected_bg` when `selected`, so the band spans the inner width even past
/// the text. `WidthBasis::Parent` gives the surface the full list width; the
/// span backgrounds are tinted too so text cells sit on the band rather than
/// leaving default-colored holes.
///
/// Columns (left to right): a right-aligned prefix in `prefix_width` plus a
/// gap, the label, then the right slot. The shortcut wins the right slot over
/// the description, matching the columns `aj` draws. When a shortcut follows,
/// the label is padded to `label_width` so shortcuts line up in a column. A
/// description keeps its original layout (label plus a two-cell gap) so rows
/// that carry no prefix or shortcut render exactly as they did before columns.
fn build_row(
    item: &SelectItem,
    selected: bool,
    styles: &SelectStyles,
    prefix_width: usize,
    label_width: usize,
) -> WidgetRef {
    let band = selected.then_some(styles.selected_bg);
    let tint = |mut style: Style| -> Style {
        if let Some(bg) = band {
            style.bg = bg;
        }
        style
    };
    let mut spans = Vec::new();
    // Right-aligned metadata column plus its gap, only when some item carries
    // a prefix. An item without one still fills the column with spaces so the
    // label stays in its aligned position.
    if prefix_width > 0 {
        let prefix = item.prefix.as_deref().unwrap_or("");
        let pad = prefix_width.saturating_sub(prefix.chars().count());
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
    if let Some(shortcut) = &item.shortcut {
        let pad = label_width.saturating_sub(item.label.chars().count());
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
    let widget: WidgetRef = Rc::new(RefCell::new(rich));
    widget
}

/// Recomputes `visible` by scoring the full item set from scratch and resets
/// the cursor to the top so it can never point past the narrowed set. The row
/// [`Builder`] is permanent, so this only refreshes the filtered view and the
/// item count.
///
/// Used by `set_items` and by `on_change` on any non-append query change.
fn full_filter(state: &mut SelectState, list: &mut ListView) {
    let SelectState {
        items,
        visible,
        query,
        matcher,
        ..
    } = state;
    // Enumerate all items in order, so `filter_scored`'s positional tiebreak
    // is the original index.
    *visible = matcher
        .filter_scored(items.iter().enumerate(), query, |(_, item)| {
            item.filter_key.as_str()
        })
        .into_iter()
        .map(|((i, _), score)| (i, score))
        .collect();
    list.item_count = Some(u32::try_from(visible.len()).expect("row count fits u32"));
    list.jump_to_item(0);
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
fn narrow_filter(state: &mut SelectState, list: &mut ListView) {
    let SelectState {
        items,
        visible,
        query,
        matcher,
        ..
    } = state;
    // Sort the candidate indices ascending before rescoring so that
    // `filter_scored`'s positional tiebreak (its internal enumeration order)
    // equals the original-index tiebreak. That makes the narrowed order
    // byte-identical to a full rescore's order over the same survivors: the
    // scores are per-item and identical either way, and ties break the same.
    let mut indices: Vec<usize> = visible.iter().map(|&(i, _)| i).collect();
    indices.sort_unstable();
    *visible = matcher
        .filter_scored(
            indices.into_iter().map(|i| (i, &items[i])),
            query,
            |(_, item)| item.filter_key.as_str(),
        )
        .into_iter()
        .map(|((i, _), score)| (i, score))
        .collect();
    list.item_count = Some(u32::try_from(visible.len()).expect("row count fits u32"));
    list.jump_to_item(0);
}

/// Extends `visible` for a streamed batch: scores only the newly appended
/// items (`items[old_len..]`) and merges them into the existing ranking,
/// avoiding a full rescore of the accumulated set. Leaves the cursor to the
/// caller (`extend_items` restores it); only the item count is updated here.
fn merge_extend(state: &mut SelectState, list: &mut ListView, old_len: usize) {
    let SelectState {
        items,
        visible,
        query,
        matcher,
        ..
    } = state;
    // Score just the new tail. `filter_scored` enumerates the tail from 0, so
    // its positional tiebreak matches ascending real index (`old_len + pos`),
    // keeping the batch's own ranking correct.
    let new_ranked: Vec<(usize, u32)> = matcher
        .filter_scored(items[old_len..].iter().enumerate(), query, |(_, item)| {
            item.filter_key.as_str()
        })
        .into_iter()
        .map(|((pos, _), score)| (old_len + pos, score))
        .collect();
    // Both lists are already in rank order (score desc, index asc), so a
    // linear two-way merge reproduces a full rescore's order. New indices all
    // exceed old ones, so a score tie between an old and a new item keeps the
    // old (lower index) first, matching the stable tiebreak.
    *visible = merge_ranked(std::mem::take(visible), new_ranked);
    list.item_count = Some(u32::try_from(visible.len()).expect("row count fits u32"));
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
/// without rebuilding the bars. The hover and drag cells are set for
/// completeness. The pick list forwards no mouse events to the bars, so
/// only the base thumb is ever drawn.
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

/// A fuzzy-filterable select list: a [`TextField`] filter row, a blank
/// separator row, and a [`ListView`] of the matching rows below.
pub struct FilterableSelect {
    filter: Rc<RefCell<TextField>>,
    /// The filter field wrapped behind the [`FILTER_MARKER`] prompt marker,
    /// drawn on the top row. Shares the field `Rc` with `filter`, which stays
    /// the focus target and owns the query and its `on_change`.
    prompt: Rc<RefCell<PromptInput>>,
    list: Rc<RefCell<ListView>>,
    /// Scroll bars wrapping the list (sharing its `Rc` via `bars.view`), for
    /// the vertical thumb. `draw` enables the vertical bar per frame only
    /// when [`Self::show_scrollbar`] is set and the list actually overflows,
    /// so a list that fits keeps the full width and shows no bar. The
    /// horizontal bar is always off (a pick list has no horizontal axis).
    bars: ScrollBars<ListView>,
    /// Whether the caller wants a vertical scroll bar when the list overflows
    /// ([`Self::set_show_scrollbar`]). The bar is still hidden while the list
    /// fits.
    show_scrollbar: bool,
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
        let mut initial = SelectState {
            visible: Vec::new(),
            items,
            query: String::new(),
            matcher: FuzzyMatcher::new(),
            prefix_width: 0,
            label_width: 0,
        };
        recompute_widths(&mut initial);
        let state = Rc::new(RefCell::new(initial));
        let styles = Rc::new(RefCell::new(styles));
        let mut list_view = ListView::new(Source::Builder(Box::new(RowBuilder {
            state: Rc::clone(&state),
            styles: Rc::clone(&styles),
        })));
        // The band replaces the arrow gutter, so the list draws no cursor
        // indicator of its own.
        list_view.draw_cursor = false;

        // Wrap the list in scroll bars for the vertical thumb. The bars own
        // the list behind their shared `view` handle, which we keep a clone
        // of for the widget's own accessors. A pick list has no horizontal
        // axis, and the vertical bar is opt-in so selectors that don't want
        // it stay pixel-identical (`draw` reserves no column while it is off).
        let mut bars = ScrollBars::new(list_view);
        bars.draw_horizontal_scrollbar = false;
        bars.draw_vertical_scrollbar = false;
        let list = Rc::clone(&bars.view);
        full_filter(&mut state.borrow_mut(), &mut list.borrow_mut());

        let filter = Rc::new(RefCell::new(TextField::new()));
        {
            let state = Rc::clone(&state);
            let list = Rc::clone(&list);
            // NOTE: this fires from the TextField's own handle_event, at
            // which point the select's capturing borrow has already been
            // released, so borrowing the shared state and list here cannot
            // collide with the widget's own borrows.
            filter.borrow_mut().on_change = Some(Box::new(move |ctx, text| {
                let mut state = state.borrow_mut();
                let mut list = list.borrow_mut();
                // A pure append can only shrink the match set (see
                // `narrow_filter`'s monotonicity invariant), so rescore just
                // the current survivors. Any other edit (backspace, paste,
                // mid-string change) may add matches, so rescore everything.
                let is_append = text.starts_with(&state.query) && text.len() > state.query.len();
                state.query = text.to_string();
                if is_append {
                    narrow_filter(&mut state, &mut list);
                } else {
                    full_filter(&mut state, &mut list);
                }
                ctx.redraw = true;
            }));
        }

        // Wrap the field behind the shared filter marker so the query reads as
        // a prompt. The field stays the focus target, so its cursor renders
        // (offset by the marker) and printables reach it.
        let prompt = Rc::new(RefCell::new(PromptInput::new(
            to_widget_ref(Rc::clone(&filter)),
            FILTER_MARKER,
            styles.borrow().marker,
        )));

        FilterableSelect {
            filter,
            prompt,
            list,
            bars,
            show_scrollbar: false,
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

    /// The widget the host should focus while this select is active: the
    /// filter field, so its cursor renders and printables edit the query.
    pub fn focus_target(&self) -> WidgetRef {
        to_widget_ref(Rc::clone(&self.filter))
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
        recompute_widths(&mut state);
        full_filter(&mut state, &mut self.list.borrow_mut());
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
        recompute_widths(&mut state);
        // Score only the new tail and merge it into the ranking, rather
        // than rescoring the whole accumulated set.
        merge_extend(&mut state, &mut self.list.borrow_mut(), old_len);
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
        // re-colors it without rebuilding the prompt.
        self.prompt.borrow_mut().marker_style = self.styles.borrow().marker;
        surface.children.push(SubSurface {
            origin: RelativePoint { row: 0, col: 0 },
            surface: draw_widget(&to_widget_ref(Rc::clone(&self.prompt)), &filter_ctx),
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
            self.bars.draw_vertical_scrollbar = self.show_scrollbar && overflow;
            // Tint the thumb from the live styles so a runtime restyle (theme
            // swap) is reflected without rebuilding the bars. The bars draw
            // the inner list (stamping its identity for wheel/key routing) and
            // reserve the rightmost column for the thumb only while the
            // vertical bar is enabled.
            apply_thumb_style(&mut self.bars, self.styles.borrow().scrollbar_thumb);
            surface.children.push(SubSurface {
                origin: RelativePoint { row: 2, col: 0 },
                surface: self.bars.draw(&list_ctx),
                z_index: 0,
            });
        }
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
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
            self.list.borrow_mut().next_item(ctx);
            return;
        }
        if key.matches(Key::UP, Modifiers::empty()) || key.matches(u32::from('p'), Modifiers::CTRL)
        {
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
            let filter = Rc::clone(&select.filter);
            filter.borrow_mut().handle_event(&mut ctx, event);
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
}

/// Helper shared by tests that hold a `SelectState` borrow directly.
#[cfg(test)]
fn visible_indices_of(state: &SelectState) -> Vec<usize> {
    state.visible.iter().map(|&(i, _)| i).collect()
}
