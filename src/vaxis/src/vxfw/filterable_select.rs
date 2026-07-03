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

use crate::cell::{Color, Style};
use crate::fuzzy::FuzzyMatcher;
use crate::key::{Key, Modifiers};
use crate::vxfw::{
    Builder, DrawContext, Event, EventContext, ListView, MaxSize, RelativePoint, RichText, Size,
    Source, SubSurface, Surface, TextField, TextSpan, Widget, WidgetRef, WidthBasis, draw_widget,
    to_widget_ref,
};

/// Theme styles for the pick list's rows, threaded from the host's palette so
/// the widget carries no theme dependency of its own.
///
/// The selection band fills the cursored row's full inner width with
/// `selected_bg`; `label` and `secondary` style the two text columns, and get
/// their background overpainted with `selected_bg` on the banded row so the
/// text sits on the band rather than punching a hole in it.
#[derive(Clone)]
pub struct SelectStyles {
    /// Background of the full-width band painted behind the cursored row.
    pub selected_bg: Color,
    /// Foreground for the primary label column.
    pub label: Style,
    /// Foreground for the secondary (description) column, typically dimmed.
    pub secondary: Style,
}

impl Default for SelectStyles {
    /// Terminal defaults with no band, for tests and callers that render
    /// unstyled.
    fn default() -> SelectStyles {
        SelectStyles {
            selected_bg: Color::Default,
            label: Style::default(),
            secondary: Style::default(),
        }
    }
}

/// One selectable row: what the list shows and what the filter matches.
///
/// The `label` and `filter_key` are separate on purpose: a row's display label
/// is often a column-formatted composite while the filter should match a
/// curated key (a category plus a human title, say) rather than layout
/// padding. `description` is an optional dim secondary column shown after the
/// label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem {
    /// Row text shown in the list.
    pub label: String,
    /// Text the fuzzy filter matches and ranks against.
    pub filter_key: String,
    /// Optional dim secondary column (a wire-level id, a one-line
    /// description), rendered to the right of the label.
    pub description: Option<String>,
}

impl SelectItem {
    /// Builds an item from its display label and filter key, with no
    /// description column.
    pub fn new(label: impl Into<String>, filter_key: impl Into<String>) -> SelectItem {
        SelectItem {
            label: label.into(),
            filter_key: filter_key.into(),
            description: None,
        }
    }

    /// Adds a dim secondary column shown after the label.
    pub fn with_description(mut self, description: impl Into<String>) -> SelectItem {
        self.description = Some(description.into());
        self
    }
}

/// The model shared between the widget, the row [`Builder`], and the filter's
/// `on_change` callback: the full item set, the filtered view onto it, and the
/// matcher.
struct SelectState {
    items: Vec<SelectItem>,
    /// Indices into `items`, filtered and ranked best-first.
    visible: Vec<usize>,
    /// The current filter text, mirrored from the `TextField` on change.
    query: String,
    matcher: FuzzyMatcher,
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
        let &item_idx = state.visible.get(idx)?;
        let styles = self.styles.borrow();
        Some(build_row(&state.items[item_idx], idx == cursor, &styles))
    }
}

/// Build one row: a full-width [`RichText`] whose cells all carry
/// `selected_bg` when `selected`, so the band spans the inner width even past
/// the text. `WidthBasis::Parent` gives the surface the full list width; the
/// span backgrounds are tinted too so text cells sit on the band rather than
/// leaving default-colored holes.
fn build_row(item: &SelectItem, selected: bool, styles: &SelectStyles) -> WidgetRef {
    let band = selected.then_some(styles.selected_bg);
    let tint = |mut style: Style| -> Style {
        if let Some(bg) = band {
            style.bg = bg;
        }
        style
    };
    let mut spans = vec![TextSpan {
        text: item.label.clone(),
        style: tint(styles.label),
        ..TextSpan::default()
    }];
    if let Some(description) = &item.description {
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

/// Recomputes `visible` from the current query and resets the cursor to the
/// top so it can never point past the narrowed set. The row [`Builder`] is
/// permanent, so this only refreshes the filtered view and the item count.
fn apply_filter(state: &mut SelectState, list: &mut ListView) {
    let SelectState {
        items,
        visible,
        query,
        matcher,
    } = state;
    *visible = matcher
        .filter(items.iter().enumerate(), query, |(_, item)| {
            item.filter_key.as_str()
        })
        .into_iter()
        .map(|(i, _)| i)
        .collect();
    list.item_count = Some(u32::try_from(visible.len()).expect("row count fits u32"));
    list.jump_to_item(0);
}

/// A fuzzy-filterable select list: a [`TextField`] filter row, a blank
/// separator row, and a [`ListView`] of the matching rows below.
pub struct FilterableSelect {
    filter: Rc<RefCell<TextField>>,
    list: Rc<RefCell<ListView>>,
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
        let state = Rc::new(RefCell::new(SelectState {
            visible: Vec::new(),
            items,
            query: String::new(),
            matcher: FuzzyMatcher::new(),
        }));
        let styles = Rc::new(RefCell::new(styles));
        let list = Rc::new(RefCell::new(ListView::new(Source::Builder(Box::new(
            RowBuilder {
                state: Rc::clone(&state),
                styles: Rc::clone(&styles),
            },
        )))));
        {
            let mut list = list.borrow_mut();
            // The band replaces the arrow gutter, so the list draws no cursor
            // indicator of its own.
            list.draw_cursor = false;
            apply_filter(&mut state.borrow_mut(), &mut list);
        }

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
                state.query = text.to_string();
                apply_filter(&mut state, &mut list.borrow_mut());
                ctx.redraw = true;
            }));
        }

        FilterableSelect {
            filter,
            list,
            state,
            styles,
            on_confirm: None,
            on_cancel: None,
        }
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
        apply_filter(&mut state, &mut self.list.borrow_mut());
    }

    /// Append `items` to the row set and re-apply the active filter,
    /// keeping the widgets and the cursor position. Used to stream in
    /// batches of an incremental scan without clearing what already
    /// showed.
    pub fn extend_items(&self, items: Vec<SelectItem>) {
        let cursor = self.list.borrow().cursor;
        {
            let mut state = self.state.borrow_mut();
            state.items.extend(items);
            apply_filter(&mut state, &mut self.list.borrow_mut());
        }
        // `apply_filter` reset the cursor to the top; restore it so a
        // streamed append doesn't yank the highlight back up.
        self.list.borrow_mut().jump_to_item(cursor);
    }

    /// Move the cursor onto the first visible item matching `pred`, used to
    /// pre-select the currently-active row on open. A no-op when nothing
    /// matches.
    pub fn select_matching(&self, pred: impl Fn(&SelectItem) -> bool) {
        let pos = {
            let state = self.state.borrow();
            state.visible.iter().position(|&i| pred(&state.items[i]))
        };
        if let Some(pos) = pos {
            self.list
                .borrow_mut()
                .jump_to_item(u32::try_from(pos).expect("pos fits u32"));
        }
    }

    /// Display labels of the filtered rows, ranked best-first.
    pub fn visible_labels(&self) -> Vec<String> {
        let state = self.state.borrow();
        state
            .visible
            .iter()
            .map(|&i| state.items[i].label.clone())
            .collect()
    }

    /// The highlighted item, or `None` while the filtered set is empty.
    pub fn selected(&self) -> Option<SelectItem> {
        let cursor = usize::try_from(self.list.borrow().cursor).expect("cursor fits usize");
        let state = self.state.borrow();
        state.visible.get(cursor).map(|&i| state.items[i].clone())
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
        surface.children.push(SubSurface {
            origin: RelativePoint { row: 0, col: 0 },
            surface: draw_widget(&to_widget_ref(Rc::clone(&self.filter)), &filter_ctx),
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
            surface.children.push(SubSurface {
                origin: RelativePoint { row: 2, col: 0 },
                surface: draw_widget(&to_widget_ref(Rc::clone(&self.list)), &list_ctx),
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
                secondary: Style::default(),
            },
        );

        let row_bgs = |select: &mut FilterableSelect| -> Vec<Vec<Color>> {
            let surface = select.draw(&draw_ctx(30, 10));
            // The list sub-surface sits at row 2; read each of its rows'
            // backgrounds across the full width.
            let list = &surface.children[1].surface;
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
}
