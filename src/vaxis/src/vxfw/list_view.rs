//! [`ListView`]: a vertically scrolling list of widgets with a movable cursor.
//!
//! # The scroll state machine
//!
//! Scrolling is applied at draw time, not when the event arrives. An event
//! only mutates the [`Scroll`] accumulators (`offset`, `pending_lines`) or the
//! `cursor`. The next [`draw`](Widget::draw) reconciles those into a concrete
//! child layout and recomputes `top`, `offset`, and `has_more`. This split is
//! load-bearing: it lets several wheel events between frames accumulate, and it
//! lets cursor moves defer the "bring the cursor into view" work to the draw,
//! where the children are actually measured.
//!
//! NOTE(D8): [`ListView`] and [`ScrollView`](crate::vxfw::ScrollView) share
//! roughly 70% of this logic but with deliberate differences. They are kept
//! separate rather than unified behind a shared engine. The list-view side:
//! `draw_cursor` defaults to true, the cursor indicator is a const (not a
//! field), there is no horizontal axis, children are bounded to the available
//! width, and only the visible `[start..end]` window is returned as children.

use std::rc::Rc;

use crate::cell::{Cell, Character};
use crate::key::{Key, Modifiers};
use crate::mouse;
use crate::vxfw::{
    DrawContext, Event, EventContext, MaxSize, RelativePoint, Size, SubSurface, Surface, Widget,
    WidgetRef, draw_widget,
};

/// Lazily provides the widget for a list index.
///
/// `idx` is the item index and `cursor` is the list's current cursor index, so
/// a source can render the cursored row differently. Returning `None` marks the
/// end of the list: the first index that yields `None` bounds the list.
pub trait ListSource {
    /// Returns the widget at `idx`, or `None` if `idx` is past the end.
    fn item(&self, idx: usize, cursor: usize) -> Option<WidgetRef>;
}

/// Where a list-style widget gets its children.
///
/// A `Slice` knows its length up front. A `Builder` is a lazy [`ListSource`],
/// used for lists too large to materialize or whose rows depend on the cursor.
pub enum Source {
    Slice(Vec<WidgetRef>),
    Builder(Box<dyn ListSource>),
}

impl Default for Source {
    fn default() -> Source {
        Source::Slice(Vec::new())
    }
}

/// Adapts a borrowed slice of widgets to the [`ListSource`] interface.
///
/// `draw` resolves a `Source::Slice` into one of these so the slice and builder
/// paths share a single `draw_builder`.
struct SliceBuilder<'a> {
    slice: &'a [WidgetRef],
}

impl ListSource for SliceBuilder<'_> {
    fn item(&self, idx: usize, _cursor: usize) -> Option<WidgetRef> {
        self.slice.get(idx).map(Rc::clone)
    }
}

/// The list-view scroll position.
///
/// Events mutate this; [`draw`](Widget::draw) reconciles it. See the module
/// docs for the draw-time contract.
struct Scroll {
    /// Index of the first fully-in-view widget.
    top: u32,
    /// Line offset within the top widget.
    offset: i32,
    /// Pending scroll amount, applied and cleared on the next draw.
    pending_lines: i32,
    /// Whether there is more room to scroll down.
    has_more: bool,
    /// The cursor must be brought into the viewport on the next draw.
    wants_cursor: bool,
}

impl Default for Scroll {
    fn default() -> Scroll {
        Scroll {
            top: 0,
            offset: 0,
            pending_lines: 0,
            has_more: true,
            wants_cursor: false,
        }
    }
}

impl Scroll {
    fn lines_down(&mut self, n: u8) -> bool {
        if !self.has_more {
            return false;
        }
        self.pending_lines += i32::from(n);
        true
    }

    fn lines_up(&mut self, n: u8) -> bool {
        if self.top == 0 && self.offset == 0 {
            return false;
        }
        self.pending_lines -= i32::from(n);
        true
    }
}

/// The indicator drawn in the cursor gutter next to the cursored row.
///
/// NOTE: This is a const in `ListView` but a field in `ScrollView`, matching
/// the upstream asymmetry.
fn cursor_indicator() -> Cell {
    Cell {
        char: Character::new("▐", 1),
        ..Cell::default()
    }
}

/// A vertically scrolling list with a movable cursor.
///
/// Construct with [`ListView::new`] and tweak the public fields. The widget is
/// stateful and interactive: it overrides [`wants_events`](Widget::wants_events)
/// and mutates its scroll position during draw.
pub struct ListView {
    pub children: Source,
    pub cursor: u32,
    /// When true, a cursor indicator is drawn next to the cursored widget.
    pub draw_cursor: bool,
    /// Lines to scroll per mouse-wheel tick.
    pub wheel_scroll: u8,
    /// Set when the exact item count is known, which lets cursor moves and
    /// jumps avoid walking the builder.
    pub item_count: Option<u32>,
    scroll: Scroll,
}

impl ListView {
    /// A list view over `children` with `draw_cursor` on and a wheel step of 3.
    pub fn new(children: Source) -> ListView {
        ListView {
            children,
            cursor: 0,
            draw_cursor: true,
            wheel_scroll: 3,
            item_count: None,
            scroll: Scroll::default(),
        }
    }

    /// Moves the cursor to the next item, bringing it into view.
    pub fn next_item(&mut self, ctx: &mut EventContext) {
        if let Some(count) = self.item_count {
            // NOTE: saturating here, where ScrollView uses a plain `count - 1`.
            if self.cursor >= count.saturating_sub(1) {
                return ctx.consume_event();
            }
            self.cursor += 1;
        } else {
            match &self.children {
                Source::Slice(slice) => {
                    let len = u32::try_from(slice.len()).expect("item count fits u32");
                    self.item_count = Some(len);
                    if self.cursor == len - 1 {
                        return ctx.consume_event();
                    }
                    self.cursor += 1;
                }
                Source::Builder(builder) => {
                    let prev = self.cursor;
                    self.cursor += 1;
                    // Walk back until we land on an item that exists, finding the
                    // last item when we stepped past the end.
                    while builder
                        .item(
                            usize::try_from(self.cursor).expect("cursor fits usize"),
                            usize::try_from(self.cursor).expect("cursor fits usize"),
                        )
                        .is_none()
                    {
                        self.cursor = self.cursor.saturating_sub(1);
                    }
                    if self.cursor == prev {
                        return ctx.consume_event();
                    }
                }
            }
        }
        self.ensure_scroll();
        ctx.consume_and_redraw();
    }

    /// Moves the cursor to the previous item, bringing it into view.
    pub fn prev_item(&mut self, ctx: &mut EventContext) {
        if self.cursor == 0 {
            return ctx.consume_event();
        }
        if let Some(count) = self.item_count {
            self.cursor = (self.cursor - 1).min(count - 1);
        } else {
            match &self.children {
                Source::Slice(slice) => {
                    let len = u32::try_from(slice.len()).expect("item count fits u32");
                    self.item_count = Some(len);
                    self.cursor = (self.cursor - 1).min(len - 1);
                }
                Source::Builder(builder) => {
                    let prev = self.cursor;
                    self.cursor -= 1;
                    while builder
                        .item(
                            usize::try_from(self.cursor).expect("cursor fits usize"),
                            usize::try_from(self.cursor).expect("cursor fits usize"),
                        )
                        .is_none()
                    {
                        self.cursor = self.cursor.saturating_sub(1);
                    }
                    if self.cursor == prev {
                        return ctx.consume_event();
                    }
                }
            }
        }
        self.ensure_scroll();
        ctx.consume_and_redraw();
    }

    /// Anchors the viewport so the cursored item is visible on the next draw.
    ///
    /// Call only after the cursor moved or to force the cursor into view. If the
    /// cursor is at or above the top we snap the top to it; otherwise we defer
    /// to the draw via `wants_cursor`.
    pub fn ensure_scroll(&mut self) {
        if self.cursor <= self.scroll.top {
            self.scroll.top = self.cursor;
            self.scroll.offset = 0;
        } else {
            self.scroll.wants_cursor = true;
        }
    }

    /// Returns the item count, caching a slice's length. Builder-backed lists
    /// without an explicit `item_count` return `None`.
    fn known_item_count(&mut self) -> Option<u32> {
        if let Some(count) = self.item_count {
            return Some(count);
        }
        match &self.children {
            Source::Slice(slice) => {
                let count = u32::try_from(slice.len()).expect("item count fits u32");
                self.item_count = Some(count);
                Some(count)
            }
            Source::Builder(_) => None,
        }
    }

    /// Moves the cursor to `idx` and starts drawing from it.
    ///
    /// Useful for large jumps: starting the draw at the cursor avoids building
    /// every child between the old and new positions. When the item count is
    /// known, `idx` is clamped to the last item.
    pub fn jump_to_item(&mut self, idx: u32) {
        let cursor = match self.known_item_count() {
            Some(0) => 0,
            Some(count) => idx.min(count - 1),
            None => idx,
        };
        self.cursor = cursor;
        self.scroll = Scroll {
            top: cursor,
            ..Scroll::default()
        };
    }

    /// Scrolls to the bottom when the item count is known, preserving the
    /// cursor. A builder-backed list without `item_count` has no known bottom,
    /// so this does nothing.
    pub fn scroll_to_bottom(&mut self) {
        let Some(count) = self.known_item_count() else {
            return;
        };
        self.scroll = if count == 0 {
            Scroll::default()
        } else {
            Scroll {
                top: count - 1,
                ..Scroll::default()
            }
        };
    }

    /// Inserts children at the front of `child_list` until `add_height` lines
    /// are filled above the current top, walking upward from `top - 1`.
    fn insert_children(
        &mut self,
        ctx: &DrawContext,
        builder: &dyn ListSource,
        child_list: &mut Vec<SubSurface>,
        add_height: i32,
    ) {
        debug_assert!(self.scroll.top > 0);
        self.scroll.top -= 1;
        let cursor = usize::try_from(self.cursor).expect("cursor fits usize");
        let max_size = ctx.max.size();
        let child_offset: u16 = if self.draw_cursor { 2 } else { 0 };
        let mut upheight = add_height;
        loop {
            let top = usize::try_from(self.scroll.top).expect("top fits usize");
            let Some(child) = builder.item(top, cursor) else {
                break;
            };
            // NOTE: plain subtraction for the width here, where the down-loop
            // saturates. Reproduced from the upstream asymmetry.
            let child_ctx = ctx.with_constraints(
                Size {
                    width: max_size.width - child_offset,
                    height: 0,
                },
                MaxSize {
                    width: Some(max_size.width - child_offset),
                    height: None,
                },
            );
            let surf = draw_widget(&child, &child_ctx);
            // Traversing backward, so accumulate before setting the origin.
            upheight -= i32::from(surf.size.height);
            child_list.insert(
                0,
                SubSurface {
                    origin: RelativePoint {
                        col: i32::from(child_offset),
                        row: upheight,
                    },
                    surface: surf,
                    z_index: 0,
                },
            );
            // Stop once we passed the top edge or reached the first item.
            if upheight <= 0 || self.scroll.top == 0 {
                break;
            }
            self.scroll.top -= 1;
        }
        // NOTE: upstream wraps this re-layout in a pair of `offset = upheight`
        // assignments with an interior `offset = 0` that the second assignment
        // immediately overrides. The only observable effect is that origins are
        // re-laid from row 0 when we overshot the top, and `offset` ends up as
        // `upheight`. We keep that effect.
        if self.scroll.top == 0 && upheight > 0 {
            let mut row: i32 = 0;
            for child in child_list.iter_mut() {
                child.origin.row = row;
                row += i32::from(child.surface.size.height);
            }
        }
        self.scroll.offset = upheight;
    }

    /// Reconciles the pending scroll into a concrete child layout.
    ///
    /// `builder` is the resolved source (a slice adapter or the user's builder).
    /// This is the heart of the state machine described in the module docs.
    fn draw_builder(&mut self, ctx: &DrawContext, builder: &dyn ListSource) -> Surface {
        let max_size = ctx.max.size();
        let cursor = usize::try_from(self.cursor).expect("cursor fits usize");

        // Assume there is more below; we only learn otherwise by running out of
        // items while drawing.
        self.scroll.has_more = true;

        let mut child_list: Vec<SubSurface> = Vec::new();

        // The accumulated height starts (offset + pending_lines) lines above the
        // top edge, so a pending downward scroll begins at a negative row and a
        // pending upward scroll begins below row 0 (to be back-filled).
        let mut accumulated_height: i32 = -(self.scroll.offset + self.scroll.pending_lines);
        self.scroll.pending_lines = 0;

        // Capture the starting index before insert_children mutates `top`.
        let mut i = usize::try_from(self.scroll.top).expect("top fits usize");

        // At the very top an upward scroll cannot consume anything, so clamp.
        if accumulated_height > 0 && self.scroll.top == 0 {
            self.scroll.offset = 0;
            accumulated_height = 0;
        }

        // Offset downward: back-fill children above the top before going down.
        if accumulated_height > 0 {
            self.insert_children(ctx, builder, &mut child_list, accumulated_height);
            let last = child_list.last().expect("insert_children added a child");
            accumulated_height = last.origin.row + i32::from(last.surface.size.height);
        }

        let child_offset: u16 = if self.draw_cursor { 2 } else { 0 };

        // The downward fill. Zig's `while (...) |x| {...} else {...}` runs the
        // `else` when the loop exhausts the optional without breaking; Rust has
        // no such construct, so we break out of a `loop` and set `has_more` on
        // the run-out path directly.
        loop {
            let Some(child) = builder.item(i, cursor) else {
                // Ran out of items: nothing more below.
                self.scroll.has_more = false;
                break;
            };
            // NOTE: saturating width here, where insert_children uses plain
            // subtraction. Reproduced from the upstream asymmetry.
            let child_ctx = ctx.with_constraints(
                Size {
                    width: max_size.width.saturating_sub(child_offset),
                    height: 0,
                },
                MaxSize {
                    width: Some(max_size.width.saturating_sub(child_offset)),
                    height: None,
                },
            );
            let surf = draw_widget(&child, &child_ctx);
            let height = i32::from(surf.size.height);
            child_list.push(SubSurface {
                origin: RelativePoint {
                    col: i32::from(child_offset),
                    row: accumulated_height,
                },
                surface: surf,
                z_index: 0,
            });
            accumulated_height += height;

            // `i < cursor` uses the pre-increment index, matching the deferred
            // increment in upstream.
            let want_more_for_cursor = self.scroll.wants_cursor && i < cursor;
            i += 1;
            if want_more_for_cursor {
                continue;
            }
            if accumulated_height >= i32::from(max_size.height) {
                break;
            }
        }

        let mut total = total_height(&child_list);

        // On a resize we may have reached the bottom without filling the screen;
        // back-fill from above to use the empty space.
        if !self.scroll.has_more && total < usize::from(max_size.height) && self.scroll.top > 0 {
            let add =
                i32::try_from(usize::from(max_size.height) - total).expect("fill height fits i32");
            self.insert_children(ctx, builder, &mut child_list, add);
            total = total_height(&child_list);
        }

        // Wrap the cursored child with the indicator gutter. The wrapper is as
        // wide as the gutter plus the child (the ScrollView variant differs).
        if self.draw_cursor && self.cursor >= self.scroll.top {
            let cursored_idx = usize::try_from(self.cursor - self.scroll.top).expect("idx fits");
            if cursored_idx < child_list.len() {
                let child = child_list[cursored_idx].clone();
                let size = child.surface.size;
                let inner = SubSurface {
                    origin: RelativePoint {
                        col: i32::from(child_offset),
                        row: 0,
                    },
                    surface: child.surface,
                    z_index: 0,
                };
                let mut cursor_surf = Surface::with_children(
                    Size {
                        width: child_offset + size.width,
                        height: size.height,
                    },
                    vec![inner],
                );
                for row in 0..cursor_surf.size.height {
                    cursor_surf.write_cell(0, row, cursor_indicator());
                }
                child_list[cursored_idx] = SubSurface {
                    origin: RelativePoint {
                        col: 0,
                        row: child.origin.row,
                    },
                    surface: cursor_surf,
                    z_index: 0,
                };
            }
        }

        // If the cursor must be in view, ensure the cursored child is fully
        // visible: anchor it to the bottom, or make it the sole top item when it
        // is taller than the viewport.
        if self.scroll.wants_cursor {
            let cursored_idx = usize::try_from(self.cursor - self.scroll.top).expect("idx fits");
            let sub_origin_row = child_list[cursored_idx].origin.row;
            let sub_height = child_list[cursored_idx].surface.size.height;
            let bottom = sub_origin_row + i32::from(sub_height);
            if bottom > i32::from(max_size.height) {
                // Anchor the cursored child (and those above it) to the bottom.
                let mut origin = i32::from(max_size.height);
                let mut idx = cursored_idx + 1;
                while idx > 0 {
                    origin -= i32::from(child_list[idx - 1].surface.size.height);
                    child_list[idx - 1].origin.row = origin;
                    idx -= 1;
                }
            } else if sub_height >= max_size.height {
                // The cursored child fills the viewport: make it the only item.
                self.scroll.top = self.cursor;
                self.scroll.offset = 0;
                let surface = child_list[cursored_idx].surface.clone();
                let h = usize::from(surface.size.height);
                child_list.clear();
                child_list.push(SubSurface {
                    origin: RelativePoint { col: 0, row: 0 },
                    surface,
                    z_index: 0,
                });
                total = h;
            }
        }

        // Reaching the bottom re-anchors the children: from the top when they do
        // not fill the screen, from the bottom otherwise.
        if !self.scroll.has_more && total < usize::from(max_size.height) {
            debug_assert!(self.scroll.top == 0);
            self.scroll.offset = 0;
            let mut origin: i32 = 0;
            for child in child_list.iter_mut() {
                child.origin.row = origin;
                origin += i32::from(child.surface.size.height);
            }
        } else if !self.scroll.has_more {
            let mut origin = i32::from(max_size.height);
            let mut idx = child_list.len();
            while idx > 0 {
                origin -= i32::from(child_list[idx - 1].surface.size.height);
                child_list[idx - 1].origin.row = origin;
                idx -= 1;
            }
        }

        // Find the visible window and recompute top/offset from the laid-out
        // origins.
        let mut start: usize = 0;
        let mut end: usize = child_list.len();
        for (idx, child) in child_list.iter().enumerate() {
            if child.origin.row <= 0 && child.origin.row + i32::from(child.surface.size.height) > 0
            {
                start = idx;
                self.scroll.offset = -child.origin.row;
                self.scroll.top += u32::try_from(idx).expect("index fits u32");
            }
            if child.origin.row > i32::from(max_size.height) {
                end = idx;
                break;
            }
        }

        // Reset the deferred cursor request now that the draw consumed it.
        self.scroll.wants_cursor = false;

        // When drawing the cursor we allocate a buffer so the list obscures any
        // content underneath it.
        let mut surface = if self.draw_cursor {
            Surface::with_size(max_size)
        } else {
            Surface {
                size: max_size,
                widget: None,
                cursor: None,
                buffer: Vec::new(),
                children: Vec::new(),
            }
        };
        // Only the visible window is returned (ScrollView returns all children).
        surface.children = child_list[start..end].to_vec();
        surface
    }
}

/// Sum of the children's heights.
fn total_height(list: &[SubSurface]) -> usize {
    list.iter()
        .map(|child| usize::from(child.surface.size.height))
        .sum()
}

impl Widget for ListView {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // NOTE: take the children out so the SliceBuilder can borrow them while
        // `draw_builder` borrows `&mut self`. We restore them before returning,
        // so this is a borrow-checker dance, not a state change.
        let children = std::mem::take(&mut self.children);
        let surface = match &children {
            Source::Slice(slice) => {
                self.item_count = Some(u32::try_from(slice.len()).expect("item count fits u32"));
                let builder = SliceBuilder { slice };
                self.draw_builder(ctx, &builder)
            }
            Source::Builder(builder) => self.draw_builder(ctx, builder.as_ref()),
        };
        self.children = children;
        surface
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            Event::Mouse(m) => {
                if m.button == mouse::Button::WheelUp && self.scroll.lines_up(self.wheel_scroll) {
                    ctx.consume_and_redraw();
                }
                if m.button == mouse::Button::WheelDown && self.scroll.lines_down(self.wheel_scroll)
                {
                    ctx.consume_and_redraw();
                }
            }
            Event::KeyPress(key) => {
                if key.matches(u32::from('j'), Modifiers::empty())
                    || key.matches(u32::from('n'), Modifiers::CTRL)
                    || key.matches(Key::DOWN, Modifiers::empty())
                {
                    self.next_item(ctx);
                    return;
                }
                if key.matches(u32::from('k'), Modifiers::empty())
                    || key.matches(u32::from('p'), Modifiers::CTRL)
                    || key.matches(Key::UP, Modifiers::empty())
                {
                    self.prev_item(ctx);
                    return;
                }
                if key.matches(Key::ESCAPE, Modifiers::empty()) {
                    self.ensure_scroll();
                    ctx.consume_and_redraw();
                }
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
    use std::cell::Cell as StdCell;
    use std::cell::RefCell;

    use super::*;
    use crate::gwidth;
    use crate::vxfw::Text;

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

    fn text(s: &str) -> WidgetRef {
        Rc::new(RefCell::new(Text::new(s)))
    }

    #[test]
    fn list_view() {
        let mut list_view = ListView::new(Source::Slice(vec![
            text("abc\n  def\n  ghi"),
            text("def"),
            text("ghi"),
            text("jkl\n mno"),
        ]));
        list_view.wheel_scroll = 1;

        let ctx = draw_ctx(16, 4);

        let mut surface = list_view.draw(&ctx);
        // ListView expands to max height and width.
        assert_eq!(surface.size.height, 4);
        assert_eq!(surface.size.width, 16);
        // Only visible children appear as surfaces.
        assert_eq!(surface.children.len(), 2);

        let mut event_ctx = EventContext::new();
        let wheel_up = Event::Mouse(mouse_event(mouse::Button::WheelUp));
        let wheel_down = Event::Mouse(mouse_event(mouse::Button::WheelDown));

        list_view.handle_event(&mut event_ctx, &wheel_up);
        // Wheel up does not adjust the scroll at the top.
        assert_eq!(list_view.scroll.top, 0);
        assert_eq!(list_view.scroll.offset, 0);

        list_view.handle_event(&mut event_ctx, &wheel_down);
        surface = list_view.draw(&ctx);
        // Down one line, top widget unchanged, one more widget in view.
        assert_eq!(list_view.scroll.top, 0);
        assert_eq!(list_view.scroll.offset, 1);
        assert_eq!(surface.children.len(), 3);

        list_view.handle_event(&mut event_ctx, &wheel_down);
        list_view.handle_event(&mut event_ctx, &wheel_down);
        surface = list_view.draw(&ctx);
        // Down two more lines scrolls the top widget out of view.
        assert_eq!(list_view.scroll.top, 1);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 3);

        list_view.handle_event(&mut event_ctx, &wheel_down);
        surface = list_view.draw(&ctx);
        // At the bottom we do not advance further.
        assert_eq!(list_view.scroll.top, 1);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 3);

        // Escape resets the viewport and brings the cursor into view.
        list_view.handle_event(
            &mut event_ctx,
            &Event::KeyPress(Key {
                codepoint: Key::ESCAPE,
                ..Key::default()
            }),
        );
        surface = list_view.draw(&ctx);
        assert_eq!(list_view.scroll.top, 0);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 2);

        let cursor_down = Event::KeyPress(Key {
            codepoint: u32::from('j'),
            ..Key::default()
        });

        list_view.handle_event(&mut event_ctx, &cursor_down);
        surface = list_view.draw(&ctx);
        // Cursor down, scroll unchanged.
        assert_eq!(list_view.scroll.top, 0);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 2);
        assert_eq!(list_view.cursor, 1);

        list_view.handle_event(&mut event_ctx, &cursor_down);
        surface = list_view.draw(&ctx);
        // Cursor down, scroll advances one row.
        assert_eq!(list_view.scroll.top, 0);
        assert_eq!(list_view.scroll.offset, 1);
        assert_eq!(surface.children.len(), 3);
        assert_eq!(list_view.cursor, 2);

        list_view.handle_event(&mut event_ctx, &cursor_down);
        surface = list_view.draw(&ctx);
        // Cursored onto the last item: the whole last item comes into view.
        assert_eq!(list_view.scroll.top, 1);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 3);
        assert_eq!(list_view.cursor, 3);
    }

    /// A builder that counts how many times it is queried, so we can assert that
    /// jumps and scroll-to-bottom do not walk every intermediate child.
    struct CountingBuilder {
        len: usize,
        widget: WidgetRef,
        calls: Rc<StdCell<usize>>,
    }

    impl ListSource for CountingBuilder {
        fn item(&self, idx: usize, _cursor: usize) -> Option<WidgetRef> {
            self.calls.set(self.calls.get() + 1);
            if idx >= self.len {
                return None;
            }
            Some(Rc::clone(&self.widget))
        }
    }

    #[test]
    fn list_view_jump_to_item_avoids_walking_intermediate_children() {
        let calls = Rc::new(StdCell::new(0usize));
        let builder = CountingBuilder {
            len: 1000,
            widget: text("item"),
            calls: Rc::clone(&calls),
        };
        let mut list_view = ListView {
            item_count: Some(1000),
            ..ListView::new(Source::Builder(Box::new(builder)))
        };

        let ctx = draw_ctx(16, 4);
        list_view.jump_to_item(999);
        let surface = list_view.draw(&ctx);

        assert_eq!(list_view.cursor, 999);
        assert_eq!(list_view.scroll.top, 996);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 4);
        assert!(calls.get() < 10);
    }

    #[test]
    fn list_view_jump_to_item_clamps_to_item_count() {
        let mut list_view = ListView {
            item_count: Some(10),
            ..ListView::new(Source::Slice(Vec::new()))
        };

        list_view.jump_to_item(100);

        assert_eq!(list_view.cursor, 9);
        assert_eq!(list_view.scroll.top, 9);
        assert_eq!(list_view.scroll.offset, 0);
    }

    #[test]
    fn list_view_scroll_to_bottom_avoids_walking_intermediate_children() {
        let calls = Rc::new(StdCell::new(0usize));
        let builder = CountingBuilder {
            len: 1000,
            widget: text("item"),
            calls: Rc::clone(&calls),
        };
        let mut list_view = ListView {
            item_count: Some(1000),
            ..ListView::new(Source::Builder(Box::new(builder)))
        };

        let ctx = draw_ctx(16, 4);
        list_view.scroll_to_bottom();
        let surface = list_view.draw(&ctx);

        assert_eq!(list_view.cursor, 0);
        assert_eq!(list_view.scroll.top, 996);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 4);
        assert!(calls.get() < 10);
    }

    #[test]
    fn list_view_scroll_to_bottom_gets_count_from_slice() {
        let mut list_view = ListView::new(Source::Slice(vec![text("0"), text("1"), text("2")]));

        list_view.scroll_to_bottom();

        assert_eq!(list_view.cursor, 0);
        assert_eq!(list_view.scroll.top, 2);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(list_view.item_count, Some(3));
    }

    #[test]
    fn list_view_uneven_scroll() {
        let mut list_view = ListView::new(Source::Slice(vec![
            text("0"),
            text("1"),
            text("2"),
            text("3"),
            text("4"),
            text("5"),
            text("6"),
        ]));
        list_view.wheel_scroll = 1;

        let ctx = draw_ctx(16, 4);
        // Initial draw to establish item_count and the scroll state.
        list_view.draw(&ctx);

        let mut event_ctx = EventContext::new();
        let wheel_down = Event::Mouse(mouse_event(mouse::Button::WheelDown));
        let wheel_up = Event::Mouse(mouse_event(mouse::Button::WheelUp));

        list_view.handle_event(&mut event_ctx, &wheel_down);
        list_view.handle_event(&mut event_ctx, &wheel_down);
        list_view.handle_event(&mut event_ctx, &wheel_down);
        let mut surface = list_view.draw(&ctx);
        assert_eq!(list_view.scroll.top, 3);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 4);

        list_view.handle_event(&mut event_ctx, &wheel_up);
        list_view.handle_event(&mut event_ctx, &wheel_up);
        surface = list_view.draw(&ctx);
        assert_eq!(list_view.scroll.top, 1);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 4);
    }

    fn mouse_event(button: mouse::Button) -> mouse::Mouse {
        mouse::Mouse {
            col: 0,
            row: 0,
            xoffset: 0,
            yoffset: 0,
            button,
            mods: mouse::Modifiers::empty(),
            kind: mouse::Type::Press,
        }
    }
}
