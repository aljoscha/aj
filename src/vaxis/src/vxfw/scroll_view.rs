//! [`ScrollView`]: a two-axis scrolling container of widgets.
//!
//! # The scroll state machine
//!
//! Like [`ListView`](crate::vxfw::ListView), scrolling is applied at draw time,
//! not when the event arrives. Events only mutate the [`Scroll`] accumulators
//! (`vertical_offset`, `pending_lines`, `left`) or the `cursor`. The next
//! [`draw`](Widget::draw) reconciles them into a concrete layout and recomputes
//! `top`, `vertical_offset`, `has_more_vertical`, and `has_more_horizontal`.
//!
//! NOTE(D8): [`ScrollView`] and [`ListView`](crate::vxfw::ListView) share most
//! of this logic but with deliberate differences, kept separate rather than
//! unified behind a shared engine. The scroll-view side: `draw_cursor` defaults
//! to false, the cursor indicator is a field (not a const), there is a
//! horizontal axis (`left` / `has_more_horizontal`), children are drawn with a
//! fully unbounded max so they keep their natural width, the cursor wrapper is
//! only the gutter wide, and every child is returned (no visible-window trim).

use std::rc::Rc;

use crate::cell::{Cell, Character};
use crate::key::{Key, Modifiers};
use crate::mouse;
use crate::vxfw::{
    DrawContext, Event, EventContext, ListSource, MaxSize, RelativePoint, Size, Source, SubSurface,
    Surface, Widget, WidgetRef, draw_widget,
};

/// Adapts a borrowed slice of widgets to the [`ListSource`] interface.
struct SliceBuilder<'a> {
    slice: &'a [WidgetRef],
}

impl ListSource for SliceBuilder<'_> {
    fn item(&self, idx: usize, _cursor: usize) -> Option<WidgetRef> {
        self.slice.get(idx).map(Rc::clone)
    }
}

/// The two-axis scroll position.
///
/// Events mutate this; [`draw`](Widget::draw) reconciles it. See the module
/// docs for the draw-time contract.
pub(crate) struct Scroll {
    /// Index of the first fully-in-view widget.
    pub(crate) top: u32,
    /// Line offset within the top widget.
    pub(crate) vertical_offset: i32,
    /// Pending vertical scroll amount, applied and cleared on the next draw.
    pub(crate) pending_lines: i32,
    /// Whether there is more room to scroll down.
    pub(crate) has_more_vertical: bool,
    /// The column of the first in-view column.
    pub(crate) left: u32,
    /// Whether there is more room to scroll right.
    pub(crate) has_more_horizontal: bool,
    /// The cursor must be brought into the viewport on the next draw.
    pub(crate) wants_cursor: bool,
}

impl Default for Scroll {
    fn default() -> Scroll {
        Scroll {
            top: 0,
            vertical_offset: 0,
            pending_lines: 0,
            has_more_vertical: true,
            left: 0,
            has_more_horizontal: true,
            wants_cursor: false,
        }
    }
}

impl Scroll {
    fn lines_down(&mut self, n: u8) -> bool {
        if !self.has_more_vertical {
            return false;
        }
        self.pending_lines += i32::from(n);
        true
    }

    fn lines_up(&mut self, n: u8) -> bool {
        if self.top == 0 && self.vertical_offset == 0 {
            return false;
        }
        self.pending_lines -= i32::from(n);
        true
    }

    fn cols_left(&mut self, n: u8) -> bool {
        if self.left == 0 {
            return false;
        }
        self.left = self.left.saturating_sub(u32::from(n));
        true
    }

    fn cols_right(&mut self, n: u8) -> bool {
        if !self.has_more_horizontal {
            return false;
        }
        self.left = self.left.saturating_add(u32::from(n));
        true
    }
}

/// A two-axis scrolling container of widgets.
///
/// Construct with [`ScrollView::new`] and tweak the public fields. The widget is
/// stateful and interactive: it overrides [`wants_events`](Widget::wants_events)
/// and mutates its scroll position during draw.
pub struct ScrollView {
    pub(crate) children: Source,
    pub(crate) cursor: u32,
    /// When true, the down/up keys move a cursor; otherwise they scroll lines.
    pub draw_cursor: bool,
    /// The cell drawn in the cursor gutter. Must be one column wide.
    pub cursor_indicator: Cell,
    /// Lines to scroll per mouse-wheel tick.
    pub wheel_scroll: u8,
    /// Set when the exact item count is known.
    pub(crate) item_count: Option<u32>,
    /// Height drawn on the last frame, used to size Ctrl-D/U half-page jumps.
    pub(crate) last_height: u8,
    pub(crate) scroll: Scroll,
}

impl ScrollView {
    /// A scroll view over `children` with `draw_cursor` off and a wheel step of
    /// 3.
    pub fn new(children: Source) -> ScrollView {
        ScrollView {
            children,
            cursor: 0,
            draw_cursor: false,
            cursor_indicator: Cell {
                char: Character::new("▐", 1),
                ..Cell::default()
            },
            wheel_scroll: 3,
            item_count: None,
            last_height: 0,
            scroll: Scroll::default(),
        }
    }

    /// Moves the cursor to the next item, bringing it into view.
    pub fn next_item(&mut self, ctx: &mut EventContext) {
        if let Some(count) = self.item_count {
            // NOTE: plain `count - 1` here, where ListView saturates.
            if self.cursor >= count - 1 {
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
    pub fn ensure_scroll(&mut self) {
        if self.cursor <= self.scroll.top {
            self.scroll.top = self.cursor;
            self.scroll.vertical_offset = 0;
        } else {
            self.scroll.wants_cursor = true;
        }
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
        let left = i32::try_from(self.scroll.left).expect("left fits i32");
        let mut upheight = add_height;
        loop {
            let top = usize::try_from(self.scroll.top).expect("top fits usize");
            let Some(child) = builder.item(top, cursor) else {
                break;
            };
            // Children are drawn with a fully unbounded max so they keep their
            // natural width for horizontal scrolling.
            let child_ctx = ctx.with_constraints(
                Size {
                    width: max_size.width - child_offset,
                    height: 0,
                },
                MaxSize {
                    width: None,
                    height: None,
                },
            );
            let surf = draw_widget(&child, &child_ctx);
            upheight -= i32::from(surf.size.height);
            child_list.insert(
                0,
                SubSurface {
                    origin: RelativePoint {
                        col: i32::from(child_offset) - left,
                        row: upheight,
                    },
                    surface: surf,
                    z_index: 0,
                },
            );
            if upheight <= 0 || self.scroll.top == 0 {
                break;
            }
            self.scroll.top -= 1;
        }
        // See the matching note in ListView::insert_children: the interior
        // `offset = 0` is overridden, leaving the origin re-layout as the only
        // effect and `vertical_offset` ending up as `upheight`.
        if self.scroll.top == 0 && upheight > 0 {
            let mut row: i32 = 0;
            for child in child_list.iter_mut() {
                child.origin.row = row;
                row += i32::from(child.surface.size.height);
            }
        }
        self.scroll.vertical_offset = upheight;
    }

    /// Reconciles the pending scroll into a concrete child layout.
    fn draw_builder(&mut self, ctx: &DrawContext, builder: &dyn ListSource) -> Surface {
        let max_size = ctx.max.size();
        let cursor = usize::try_from(self.cursor).expect("cursor fits usize");
        let left = i32::try_from(self.scroll.left).expect("left fits i32");

        self.scroll.has_more_vertical = true;

        let mut child_list: Vec<SubSurface> = Vec::new();

        let mut accumulated_height: i32 =
            -(self.scroll.vertical_offset + self.scroll.pending_lines);
        self.scroll.pending_lines = 0;

        let mut i = usize::try_from(self.scroll.top).expect("top fits usize");

        if accumulated_height > 0 && self.scroll.top == 0 {
            self.scroll.vertical_offset = 0;
            accumulated_height = 0;
        }

        if accumulated_height > 0 {
            self.insert_children(ctx, builder, &mut child_list, accumulated_height);
            let last = child_list.last().expect("insert_children added a child");
            accumulated_height = last.origin.row + i32::from(last.surface.size.height);
        }

        let child_offset: u16 = if self.draw_cursor { 2 } else { 0 };

        // The downward fill. Zig's `while (...) |x| {...} else {...}` has no Rust
        // equivalent, so we break out of a `loop` and set `has_more_vertical` on
        // the run-out path directly.
        loop {
            let Some(child) = builder.item(i, cursor) else {
                self.scroll.has_more_vertical = false;
                break;
            };
            let child_ctx = ctx.with_constraints(
                Size {
                    width: max_size.width - child_offset,
                    height: 0,
                },
                MaxSize {
                    width: None,
                    height: None,
                },
            );
            let surf = draw_widget(&child, &child_ctx);
            let height = i32::from(surf.size.height);
            child_list.push(SubSurface {
                origin: RelativePoint {
                    col: i32::from(child_offset) - left,
                    row: accumulated_height,
                },
                surface: surf,
                z_index: 0,
            });
            accumulated_height += height;

            let want_more_for_cursor = self.scroll.wants_cursor && i < cursor;
            i += 1;
            if want_more_for_cursor {
                continue;
            }
            if accumulated_height >= i32::from(max_size.height) {
                break;
            }
        }

        // If we filled the screen without running out, peek one item past the
        // last drawn one. If it does not exist we just drew the final item, so
        // there is nothing more below.
        if self.scroll.has_more_vertical && accumulated_height <= i32::from(max_size.height) {
            if builder.item(i, cursor).is_none() {
                self.scroll.has_more_vertical = false;
            }
        }

        let mut total = total_height(&child_list);

        if !self.scroll.has_more_vertical
            && total < usize::from(max_size.height)
            && self.scroll.top > 0
        {
            let add =
                i32::try_from(usize::from(max_size.height) - total).expect("fill height fits i32");
            self.insert_children(ctx, builder, &mut child_list, add);
            total = total_height(&child_list);
        }

        // Wrap the cursored child with the indicator gutter. The wrapper is only
        // the gutter wide (the ListView variant also spans the child).
        if self.draw_cursor && self.cursor >= self.scroll.top {
            let cursored_idx = usize::try_from(self.cursor - self.scroll.top).expect("idx fits");
            if cursored_idx < child_list.len() {
                let child = child_list[cursored_idx].clone();
                let child_height = child.surface.size.height;
                let inner = SubSurface {
                    origin: RelativePoint {
                        col: i32::from(child_offset) - left,
                        row: 0,
                    },
                    surface: child.surface,
                    z_index: 0,
                };
                let mut cursor_surf = Surface::with_children(
                    Size {
                        width: child_offset,
                        height: child_height,
                    },
                    vec![inner],
                );
                for row in 0..cursor_surf.size.height {
                    cursor_surf.write_cell(0, row, self.cursor_indicator.clone());
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

        if self.scroll.wants_cursor {
            let cursored_idx = usize::try_from(self.cursor - self.scroll.top).expect("idx fits");
            let sub_origin_row = child_list[cursored_idx].origin.row;
            let sub_height = child_list[cursored_idx].surface.size.height;
            let bottom = sub_origin_row + i32::from(sub_height);
            if bottom > i32::from(max_size.height) {
                let mut origin = i32::from(max_size.height);
                let mut idx = cursored_idx + 1;
                while idx > 0 {
                    origin -= i32::from(child_list[idx - 1].surface.size.height);
                    child_list[idx - 1].origin.row = origin;
                    idx -= 1;
                }
            } else if sub_height >= max_size.height {
                self.scroll.top = self.cursor;
                self.scroll.vertical_offset = 0;
                let surface = child_list[cursored_idx].surface.clone();
                let h = usize::from(surface.size.height);
                child_list.clear();
                child_list.push(SubSurface {
                    origin: RelativePoint { col: -left, row: 0 },
                    surface,
                    z_index: 0,
                });
                total = h;
            }
        }

        if !self.scroll.has_more_vertical && total < usize::from(max_size.height) {
            debug_assert!(self.scroll.top == 0);
            self.scroll.vertical_offset = 0;
            let mut origin: i32 = 0;
            for child in child_list.iter_mut() {
                child.origin.row = origin;
                origin += i32::from(child.surface.size.height);
            }
        } else if !self.scroll.has_more_vertical {
            let mut origin = i32::from(max_size.height);
            let mut idx = child_list.len();
            while idx > 0 {
                origin -= i32::from(child_list[idx - 1].surface.size.height);
                child_list[idx - 1].origin.row = origin;
                idx -= 1;
            }
        }

        // Recompute horizontal overflow from the laid-out widths.
        self.scroll.has_more_horizontal = false;
        for child in &child_list {
            if u32::from(child.surface.size.width).saturating_sub(self.scroll.left)
                > u32::from(max_size.width)
            {
                self.scroll.has_more_horizontal = true;
                break;
            }
        }

        // Recompute top/vertical_offset from the laid-out origins. ScrollView
        // keeps every child, so unlike ListView there is no visible-window trim
        // here. We still walk the children to update top/vertical_offset, and to
        // stop updating once we pass the bottom edge.
        for (idx, child) in child_list.iter().enumerate() {
            if child.origin.row <= 0 && child.origin.row + i32::from(child.surface.size.height) > 0
            {
                self.scroll.vertical_offset = -child.origin.row;
                self.scroll.top += u32::try_from(idx).expect("index fits u32");
            }
            if child.origin.row > i32::from(max_size.height) {
                break;
            }
        }

        self.scroll.wants_cursor = false;

        // Update the height for the next half-page jump, clamped to a u8.
        self.last_height = if total > 255 {
            255
        } else {
            u8::try_from(total).expect("total <= 255")
        };

        Surface {
            size: max_size,
            widget: None,
            cursor: None,
            buffer: Vec::new(),
            children: child_list,
        }
    }
}

/// Sum of the children's heights.
fn total_height(list: &[SubSurface]) -> usize {
    list.iter()
        .map(|child| usize::from(child.surface.size.height))
        .sum()
}

impl Widget for ScrollView {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // See ListView::draw: take the children out so the SliceBuilder can
        // borrow them while `draw_builder` borrows `&mut self`.
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
                    return ctx.consume_and_redraw();
                }
                if m.button == mouse::Button::WheelDown && self.scroll.lines_down(self.wheel_scroll)
                {
                    return ctx.consume_and_redraw();
                }
                // The horizontal wheel is inverted: wheel-left moves the view
                // right (scrolls content left under the viewport).
                if m.button == mouse::Button::WheelLeft && self.scroll.cols_right(self.wheel_scroll)
                {
                    return ctx.consume_and_redraw();
                }
                if m.button == mouse::Button::WheelRight && self.scroll.cols_left(self.wheel_scroll)
                {
                    ctx.consume_and_redraw();
                }
            }
            Event::KeyPress(key) => {
                if key.matches(Key::DOWN, Modifiers::empty())
                    || key.matches(u32::from('j'), Modifiers::empty())
                    || key.matches(u32::from('n'), Modifiers::CTRL)
                {
                    if self.draw_cursor {
                        self.next_item(ctx);
                        return;
                    }
                    if self.scroll.lines_down(1) {
                        ctx.consume_and_redraw();
                    }
                }
                if key.matches(Key::UP, Modifiers::empty())
                    || key.matches(u32::from('k'), Modifiers::empty())
                    || key.matches(u32::from('p'), Modifiers::CTRL)
                {
                    if self.draw_cursor {
                        self.prev_item(ctx);
                        return;
                    }
                    if self.scroll.lines_up(1) {
                        ctx.consume_and_redraw();
                    }
                }
                if key.matches(Key::RIGHT, Modifiers::empty())
                    || key.matches(u32::from('l'), Modifiers::empty())
                    || key.matches(u32::from('f'), Modifiers::CTRL)
                {
                    if self.scroll.cols_right(1) {
                        ctx.consume_and_redraw();
                    }
                }
                if key.matches(Key::LEFT, Modifiers::empty())
                    || key.matches(u32::from('h'), Modifiers::empty())
                    || key.matches(u32::from('b'), Modifiers::CTRL)
                {
                    if self.scroll.cols_left(1) {
                        ctx.consume_and_redraw();
                    }
                }
                if key.matches(u32::from('d'), Modifiers::CTRL) {
                    let scroll_lines = (self.last_height / 2).max(1);
                    if self.scroll.lines_down(scroll_lines) {
                        ctx.consume_and_redraw();
                    }
                }
                if key.matches(u32::from('u'), Modifiers::CTRL) {
                    let scroll_lines = (self.last_height / 2).max(1);
                    if self.scroll.lines_up(scroll_lines) {
                        ctx.consume_and_redraw();
                    }
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

    fn mouse_event(button: mouse::Button) -> Event {
        Event::Mouse(mouse::Mouse {
            col: 0,
            row: 0,
            xoffset: 0,
            yoffset: 0,
            button,
            mods: mouse::Modifiers::empty(),
            kind: mouse::Type::Press,
        })
    }

    fn key(cp: char, mods: Modifiers) -> Event {
        Event::KeyPress(Key {
            codepoint: u32::from(cp),
            mods,
            ..Key::default()
        })
    }

    #[test]
    fn scroll_view() {
        let mut sv = ScrollView::new(Source::Slice(vec![
            text("abc\n  def\n  ghi"),
            text("def"),
            text("ghi"),
            text("jkl\n mno"),
        ]));
        sv.wheel_scroll = 1;

        let ctx = draw_ctx(3, 4);

        let mut surface = sv.draw(&ctx);
        assert_eq!(surface.size.height, 4);
        assert_eq!(surface.size.width, 3);
        assert_eq!(surface.children.len(), 2);

        assert_eq!(sv.scroll.top, 0);
        assert_eq!(sv.scroll.left, 0);
        assert!(sv.scroll.has_more_vertical);
        assert!(sv.scroll.has_more_horizontal);

        let mut ec = EventContext::new();

        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelUp));
        assert_eq!(sv.scroll.top, 0);
        assert_eq!(sv.scroll.vertical_offset, 0);

        // Wheel right does not adjust the horizontal scroll at the left edge.
        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelRight));
        assert_eq!(sv.scroll.left, 0);

        // 'h' does not adjust the horizontal scroll at the left edge.
        sv.handle_event(&mut ec, &key('h', Modifiers::empty()));
        assert_eq!(sv.scroll.left, 0);

        // Ctrl-c does not adjust the horizontal scroll.
        sv.handle_event(&mut ec, &key('c', Modifiers::CTRL));
        assert_eq!(sv.scroll.left, 0);

        // === SCROLL DOWN ===

        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelDown));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.top, 0);
        assert_eq!(sv.scroll.vertical_offset, 1);
        assert_eq!(surface.children.len(), 3);

        sv.handle_event(&mut ec, &key('j', Modifiers::empty()));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.top, 0);
        assert_eq!(sv.scroll.vertical_offset, 2);
        assert_eq!(surface.children.len(), 4);

        sv.handle_event(&mut ec, &key('n', Modifiers::CTRL));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.top, 1);
        assert_eq!(sv.scroll.vertical_offset, 0);
        assert_eq!(surface.children.len(), 4);
        assert!(!sv.scroll.has_more_vertical);

        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelDown));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.top, 1);
        assert_eq!(sv.scroll.vertical_offset, 0);
        assert_eq!(surface.children.len(), 3);
        assert!(!sv.scroll.has_more_vertical);

        // === SCROLL UP ===

        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelUp));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.top, 0);
        assert_eq!(sv.scroll.vertical_offset, 2);
        assert_eq!(surface.children.len(), 4);
        assert!(sv.scroll.has_more_vertical);

        sv.handle_event(&mut ec, &key('k', Modifiers::empty()));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.top, 0);
        assert_eq!(sv.scroll.vertical_offset, 1);
        assert_eq!(surface.children.len(), 3);

        sv.handle_event(&mut ec, &key('p', Modifiers::CTRL));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.top, 0);
        assert_eq!(sv.scroll.vertical_offset, 0);
        assert_eq!(surface.children.len(), 2);
        assert_eq!(sv.scroll.top, 0);
        assert_eq!(sv.scroll.left, 0);

        // === SCROLL LEFT (moves view to the right) ===

        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelLeft));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.left, 1);
        assert_eq!(surface.children.len(), 2);
        assert!(sv.scroll.has_more_horizontal);

        sv.handle_event(&mut ec, &key('l', Modifiers::empty()));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.left, 2);
        assert_eq!(surface.children.len(), 2);
        assert!(!sv.scroll.has_more_horizontal);

        // Ctrl-f does nothing here: there is no more to scroll right.
        sv.handle_event(&mut ec, &key('f', Modifiers::CTRL));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.left, 2);
        assert_eq!(surface.children.len(), 2);
        assert!(!sv.scroll.has_more_horizontal);

        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelRight));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.left, 1);
        assert_eq!(surface.children.len(), 2);
        assert!(sv.scroll.has_more_horizontal);

        // Two events before a draw can overscroll, because we only learn there
        // is no more horizontal room after drawing.
        sv.handle_event(&mut ec, &key('f', Modifiers::CTRL));
        sv.handle_event(&mut ec, &key('l', Modifiers::empty()));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.left, 3);
        assert_eq!(surface.children.len(), 2);
        assert!(!sv.scroll.has_more_horizontal);

        // === SCROLL RIGHT (moves view to the left) ===

        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelRight));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.left, 2);
        assert_eq!(surface.children.len(), 2);
        assert!(!sv.scroll.has_more_horizontal);

        sv.handle_event(&mut ec, &key('h', Modifiers::empty()));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.left, 1);
        assert_eq!(surface.children.len(), 2);
        assert!(sv.scroll.has_more_horizontal);

        sv.handle_event(&mut ec, &key('b', Modifiers::CTRL));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.left, 0);
        assert_eq!(surface.children.len(), 2);
        assert!(sv.scroll.has_more_horizontal);

        // === COMBINED HORIZONTAL AND VERTICAL ===

        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelLeft));
        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelLeft));
        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelLeft));
        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelDown));
        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelDown));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.left, 3);
        assert_eq!(sv.scroll.top, 0);
        assert_eq!(sv.scroll.vertical_offset, 2);
        assert_eq!(surface.children.len(), 4);
        assert!(!sv.scroll.has_more_horizontal);
    }

    #[test]
    fn scroll_view_uneven_scroll() {
        let mut sv = ScrollView::new(Source::Slice(vec![
            text("0"),
            text("1"),
            text("2"),
            text("3"),
            text("4"),
            text("5"),
            text("6"),
        ]));
        sv.wheel_scroll = 1;

        let ctx = draw_ctx(16, 4);
        // Initial draw to establish item_count and the scroll state.
        sv.draw(&ctx);

        let mut ec = EventContext::new();

        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelDown));
        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelDown));
        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelDown));
        let mut surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.top, 3);
        assert_eq!(sv.scroll.vertical_offset, 0);
        // Pending scroll keeps every child drawn this frame.
        assert_eq!(surface.children.len(), 7);

        surface = sv.draw(&ctx);
        // Drawing again with no pending events leaves only the visible items.
        assert_eq!(surface.children.len(), 4);

        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelUp));
        sv.handle_event(&mut ec, &mouse_event(mouse::Button::WheelUp));
        surface = sv.draw(&ctx);
        assert_eq!(sv.scroll.top, 1);
        assert_eq!(sv.scroll.vertical_offset, 0);
        assert_eq!(surface.children.len(), 4);
    }
}
