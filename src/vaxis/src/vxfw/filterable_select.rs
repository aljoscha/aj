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
//! Outcomes flow through the `on_confirm` / `on_cancel` callbacks. The widget
//! owns no result state.

use std::cell::RefCell;
use std::rc::Rc;

use crate::fuzzy::FuzzyMatcher;
use crate::key::{Key, Modifiers};
use crate::vxfw::{
    DrawContext, Event, EventContext, ListView, MaxSize, RelativePoint, Size, Source, SubSurface,
    Surface, Text, TextField, Widget, WidgetRef, draw_widget, to_widget_ref,
};

/// One selectable row: what the list shows and what the filter matches.
///
/// The two are separate on purpose: a row's display label is often a
/// column-formatted composite while the filter should match a curated key
/// (a category plus a human title, say) rather than layout padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem {
    /// Row text shown in the list.
    pub label: String,
    /// Text the fuzzy filter matches and ranks against.
    pub filter_key: String,
}

impl SelectItem {
    /// Builds an item from its display label and filter key.
    pub fn new(label: impl Into<String>, filter_key: impl Into<String>) -> SelectItem {
        SelectItem {
            label: label.into(),
            filter_key: filter_key.into(),
        }
    }
}

/// The model shared between the widget and the filter's `on_change`
/// callback: the full item set, the filtered view onto it, and the matcher.
struct SelectState {
    items: Vec<SelectItem>,
    /// Indices into `items`, filtered and ranked best-first.
    visible: Vec<usize>,
    /// The current filter text, mirrored from the `TextField` on change.
    query: String,
    matcher: FuzzyMatcher,
}

/// Recomputes `visible` from the current query and rebuilds the list's rows,
/// resetting the cursor to the top so it can never point past the narrowed
/// set.
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
    let rows: Vec<WidgetRef> = visible
        .iter()
        .map(|&i| {
            let text: WidgetRef = Rc::new(RefCell::new(Text::new(&items[i].label)));
            text
        })
        .collect();
    list.item_count = Some(u32::try_from(rows.len()).expect("row count fits u32"));
    list.children = Source::Slice(rows);
    list.jump_to_item(0);
}

/// A fuzzy-filterable select list: a [`TextField`] filter row, a blank
/// separator row, and a [`ListView`] of the matching rows below.
pub struct FilterableSelect {
    filter: Rc<RefCell<TextField>>,
    list: Rc<RefCell<ListView>>,
    state: Rc<RefCell<SelectState>>,
    /// Fires on Enter/Ctrl+J with the highlighted item. No-op while the
    /// filtered set is empty.
    pub on_confirm: Option<Box<dyn FnMut(&mut EventContext, &SelectItem)>>,
    /// Fires on Escape.
    pub on_cancel: Option<Box<dyn FnMut(&mut EventContext)>>,
}

impl FilterableSelect {
    /// A select over `items`, initially unfiltered with the cursor on the
    /// first row.
    pub fn new(items: Vec<SelectItem>) -> FilterableSelect {
        let state = Rc::new(RefCell::new(SelectState {
            visible: Vec::new(),
            items,
            query: String::new(),
            matcher: FuzzyMatcher::new(),
        }));
        let list = Rc::new(RefCell::new(ListView::new(Source::Slice(Vec::new()))));
        apply_filter(&mut state.borrow_mut(), &mut list.borrow_mut());

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
        FilterableSelect::new(vec![
            SelectItem::new("alpha", "alpha"),
            SelectItem::new("bravo", "bravo"),
            SelectItem::new("charlie", "charlie"),
        ])
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
        let ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(30),
                height: Some(10),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        };
        let surface = select.draw(&ctx);
        assert_eq!(surface.size.width, 30);
        assert_eq!(surface.size.height, 10);
        assert_eq!(surface.children.len(), 2);
        // Filter row on top, list below the blank separator row.
        assert_eq!(surface.children[0].origin, RelativePoint { row: 0, col: 0 });
        assert_eq!(surface.children[0].surface.size.height, 1);
        assert_eq!(surface.children[1].origin, RelativePoint { row: 2, col: 0 });
        assert_eq!(surface.children[1].surface.size.height, 8);
    }
}
