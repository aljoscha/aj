//! [`ScrollBars`]: wraps a scrollable view and draws draggable scroll bars.
//!
//! The wrapped view is anything implementing [`ScrollableView`], by default a
//! [`ScrollView`]. This is the only widget that overrides
//! [`capture_event`](Widget::capture_event): while a thumb is being dragged it
//! intercepts the drag and release in the capturing phase, before they reach
//! the inner content, and translates the thumb position into a scroll position
//! via the view's [`ScrollableView`] scroll accessors.
//!
//! The bars are sized with floating-point proportions of an estimated content
//! extent. When no estimate is given we fall back to the number and width of the
//! children the [`ScrollView`] actually rendered, which is less stable across
//! frames but needs no caller input.

use std::cell::RefCell;
use std::rc::Rc;

use crate::cell::{Cell, Character, Color, Style};
use crate::mouse;
use crate::vxfw::scroll_view::ScrollView;
use crate::vxfw::{
    DrawContext, Event, EventContext, RelativePoint, Size, SubSurface, Surface, Widget,
    draw_widget, to_widget_ref,
};

/// Floating-point conversions for the thumb geometry.
///
/// The `as` casts are unavoidable: Rust has no `From` between these integer
/// widths and `f32`. The values are small screen coordinates and item counts
/// well within `f32`'s exact range, and the float math mirrors upstream's
/// proportional thumb sizing.
mod num {
    #[allow(clippy::as_conversions)]
    pub(super) fn u32_to_f32(v: u32) -> f32 {
        v as f32
    }
    #[allow(clippy::as_conversions)]
    pub(super) fn usize_to_f32(v: usize) -> f32 {
        v as f32
    }
    #[allow(clippy::as_conversions)]
    pub(super) fn f32_to_u32(v: f32) -> u32 {
        v as u32
    }
    #[allow(clippy::as_conversions)]
    pub(super) fn f32_to_u16(v: f32) -> u16 {
        v as u16
    }
}

/// What [`ScrollBars`] needs from the view it wraps.
///
/// The bars size and place their thumbs from the total item count and the
/// scroll position the view reconciled on its last draw, and drag-to-jump
/// writes that position back directly. A view without a horizontal axis
/// reports zero and no-more on the horizontal accessors and ignores
/// horizontal jumps.
pub trait ScrollableView: Widget {
    /// Total number of items, walking the source when it is not known up
    /// front.
    fn total_item_count(&self) -> usize;
    /// Index of the top in-view item, as of the last draw.
    fn scroll_top(&self) -> u32;
    /// Whether more content lies below the viewport, as of the last draw.
    fn has_more_below(&self) -> bool;
    /// Jumps the viewport so `top` is the first in-view item.
    fn set_scroll_top(&mut self, top: u32);
    /// Left column of the viewport, as of the last draw.
    fn scroll_left(&self) -> u32;
    /// Whether more content lies right of the viewport, as of the last draw.
    fn has_more_right(&self) -> bool;
    /// Jumps the viewport to start at column `left`.
    fn set_scroll_left(&mut self, left: u32);
}

/// A scrollable view with draggable, hoverable scroll bars.
///
/// The wrapped view is held behind an `Rc<RefCell<V>>` so it has a stable
/// widget identity. [`draw`](Widget::draw) stamps the inner view's surface
/// with that identity via [`draw_widget`] and appends it as a child, so the
/// event bus hit-tests and routes wheel and key events to the inner view. The
/// bars' own hover and thumb-drag interaction stays in this widget's
/// [`handle_event`](Widget::handle_event) and
/// [`capture_event`](Widget::capture_event), reaching into the view through
/// [`ScrollableView`].
pub struct ScrollBars<V: ScrollableView + 'static = ScrollView> {
    /// The wrapped view. The bars are drawn for this view, and its widget
    /// identity is stamped so the bus routes scroll events to it.
    pub view: Rc<RefCell<V>>,
    /// Whether to draw the horizontal scroll bar.
    pub draw_horizontal_scrollbar: bool,
    /// Whether to draw the vertical scroll bar.
    pub draw_vertical_scrollbar: bool,
    /// Estimated total content height, used to size the vertical thumb. Falls
    /// back to the rendered child count when `None`.
    pub estimated_content_height: Option<u32>,
    /// Estimated total content width, used to size the horizontal thumb. Falls
    /// back to the rendered child widths when `None`.
    pub estimated_content_width: Option<u32>,
    pub vertical_scrollbar_thumb: Cell,
    pub vertical_scrollbar_hover_thumb: Cell,
    pub vertical_scrollbar_drag_thumb: Cell,
    pub horizontal_scrollbar_thumb: Cell,
    pub horizontal_scrollbar_hover_thumb: Cell,
    pub horizontal_scrollbar_drag_thumb: Cell,

    // Private interaction state, recomputed each frame and across drags.
    last_frame_size: Size,
    last_frame_max_content_width: u32,
    mouse_offset_into_thumb: u8,
    vertical_thumb_top_row: u32,
    vertical_thumb_bottom_row: u32,
    is_hovering_vertical_thumb: bool,
    is_dragging_vertical_thumb: bool,
    horizontal_thumb_start_col: u32,
    horizontal_thumb_end_col: u32,
    is_hovering_horizontal_thumb: bool,
    is_dragging_horizontal_thumb: bool,
}

fn thumb(grapheme: &str) -> Cell {
    Cell {
        char: Character::new(grapheme, 1),
        ..Cell::default()
    }
}

fn drag_thumb(grapheme: &str) -> Cell {
    Cell {
        char: Character::new(grapheme, 1),
        style: Style {
            fg: Color::Index(4),
            ..Style::default()
        },
        ..Cell::default()
    }
}

impl<V: ScrollableView + 'static> ScrollBars<V> {
    /// Wraps `view` with both bars enabled and the default thumb cells.
    pub fn new(view: V) -> ScrollBars<V> {
        ScrollBars {
            view: Rc::new(RefCell::new(view)),
            draw_horizontal_scrollbar: true,
            draw_vertical_scrollbar: true,
            estimated_content_height: None,
            estimated_content_width: None,
            vertical_scrollbar_thumb: thumb("▐"),
            vertical_scrollbar_hover_thumb: thumb("█"),
            vertical_scrollbar_drag_thumb: drag_thumb("█"),
            horizontal_scrollbar_thumb: thumb("▃"),
            horizontal_scrollbar_hover_thumb: thumb("█"),
            horizontal_scrollbar_drag_thumb: drag_thumb("█"),
            last_frame_size: Size {
                width: 0,
                height: 0,
            },
            last_frame_max_content_width: 0,
            mouse_offset_into_thumb: 0,
            vertical_thumb_top_row: 0,
            vertical_thumb_bottom_row: 0,
            is_hovering_vertical_thumb: false,
            is_dragging_vertical_thumb: false,
            horizontal_thumb_start_col: 0,
            horizontal_thumb_end_col: 0,
            is_hovering_horizontal_thumb: false,
            is_dragging_horizontal_thumb: false,
        }
    }
}

impl<V: ScrollableView + 'static> Widget for ScrollBars<V> {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let mut children: Vec<SubSurface> = Vec::new();

        // A stable identity for the inner view so `draw_widget` can stamp its
        // surface and the bus routes wheel and key events to it.
        let scroll_view_ref = to_widget_ref(Rc::clone(&self.view));

        // No bars: draw the scroll view directly.
        if !self.draw_vertical_scrollbar && !self.draw_horizontal_scrollbar {
            children.push(SubSurface {
                origin: RelativePoint { row: 0, col: 0 },
                surface: draw_widget(&scroll_view_ref, ctx),
                z_index: 0,
            });
            return Surface {
                size: ctx.max.size(),
                widget: None,
                cursor: None,
                buffer: Vec::new(),
                children,
            };
        }

        let max = ctx.max.size();
        self.last_frame_size = max;

        // Draw the scroll view, leaving room for whichever bars are drawn.
        let scroll_view_surface = draw_widget(
            &scroll_view_ref,
            &ctx.with_constraints(
                ctx.min,
                crate::vxfw::MaxSize {
                    width: Some(
                        max.width
                            .saturating_sub(u16::from(self.draw_vertical_scrollbar)),
                    ),
                    height: Some(
                        max.height
                            .saturating_sub(u16::from(self.draw_horizontal_scrollbar)),
                    ),
                },
            ),
        );
        let rendered_children = scroll_view_surface.children.len();
        let max_rendered_width = scroll_view_surface
            .children
            .iter()
            .map(|child| u32::from(child.surface.size.width))
            .max()
            .unwrap_or(0);
        let scroll_view_height = scroll_view_surface.size.height;
        children.push(SubSurface {
            origin: RelativePoint { row: 0, col: 0 },
            surface: scroll_view_surface,
            z_index: 0,
        });

        // Vertical scroll bar. Read the reconciled scroll state through the
        // shared handle, the bar and thumb geometry derive from it.
        let (scroll_top, has_more_vertical) = {
            let view = self.view.borrow();
            (view.scroll_top(), view.has_more_below())
        };
        if self.draw_vertical_scrollbar && !(scroll_top == 0 && !has_more_vertical) {
            // The bar spans the scroll view, which is one row shorter than the
            // widget when the horizontal bar is drawn. The thumb must be placed
            // within this height, not the full `max.height`, or its bottom row
            // is clipped away when pinned to the end.
            let bar_height = max
                .height
                .saturating_sub(u16::from(self.draw_horizontal_scrollbar));
            let widget_height_f = f32::from(scroll_view_height);
            let total_num_children_f = num::usize_to_f32(self.view.borrow().total_item_count());

            let thumb_height: u16 = if let Some(h) = self.estimated_content_height {
                let content_height_f = num::u32_to_f32(h);
                let thumb_height_f = widget_height_f * widget_height_f / content_height_f;
                num::f32_to_u16(thumb_height_f.max(1.0))
            } else {
                let num_children_rendered_f = num::usize_to_f32(rendered_children);
                let thumb_height_f =
                    widget_height_f * num_children_rendered_f / total_num_children_f;
                num::f32_to_u16(thumb_height_f.max(1.0))
            };

            let thumb_top: u32 = if scroll_top == 0 {
                0
            } else if has_more_vertical {
                let top_child_idx_f = num::u32_to_f32(scroll_top);
                let thumb_top_f = widget_height_f * top_child_idx_f / total_num_children_f;
                num::f32_to_u32(thumb_top_f)
            } else {
                u32::from(bar_height.saturating_sub(thumb_height))
            };

            let mut scroll_bar = Surface::with_size(Size {
                width: 1,
                height: bar_height,
            });
            let cell = if self.is_dragging_vertical_thumb {
                self.vertical_scrollbar_drag_thumb.clone()
            } else if self.is_hovering_vertical_thumb {
                self.vertical_scrollbar_hover_thumb.clone()
            } else {
                self.vertical_scrollbar_thumb.clone()
            };
            let thumb_end_row = thumb_top + u32::from(thumb_height);
            for row in thumb_top..thumb_end_row {
                scroll_bar.write_cell(
                    0,
                    u16::try_from(row).expect("thumb row fits u16"),
                    cell.clone(),
                );
            }
            self.vertical_thumb_top_row = thumb_top;
            self.vertical_thumb_bottom_row = thumb_end_row;
            children.push(SubSurface {
                origin: RelativePoint {
                    row: 0,
                    col: i32::from(max.width.saturating_sub(1)),
                },
                surface: scroll_bar,
                z_index: 0,
            });
        }

        // Horizontal scroll bar. Drawn only when there is horizontal content to
        // reach, either because we are scrolled right or there is more to show.
        let (scroll_left, has_more_horizontal) = {
            let view = self.view.borrow();
            (view.scroll_left(), view.has_more_right())
        };
        let should_draw_horizontal = scroll_left > 0 || has_more_horizontal;
        if self.draw_horizontal_scrollbar && should_draw_horizontal {
            let widget_width_f = f32::from(max.width);

            let max_content_width: u32 = self.estimated_content_width.unwrap_or(max_rendered_width);

            let max_content_width_f = if scroll_left + u32::from(max.width) > max_content_width {
                // Overscrolled (e.g. the content changed): widen the content
                // so the thumb does not vanish.
                num::u32_to_f32(scroll_left + u32::from(max.width))
            } else {
                num::u32_to_f32(max_content_width)
            };
            self.last_frame_max_content_width = max_content_width;

            let thumb_width_f = widget_width_f * widget_width_f / max_content_width_f;
            let thumb_width = num::f32_to_u32(thumb_width_f.max(1.0));

            let view_start_col_f = num::u32_to_f32(scroll_left);
            let thumb_start_f = view_start_col_f * widget_width_f / max_content_width_f;
            let thumb_start = num::f32_to_u32(thumb_start_f);
            let thumb_end = thumb_start + thumb_width;

            let mut scroll_bar = Surface::with_size(Size {
                width: max.width,
                height: 1,
            });
            let cell = if self.is_dragging_horizontal_thumb {
                self.horizontal_scrollbar_drag_thumb.clone()
            } else if self.is_hovering_horizontal_thumb {
                self.horizontal_scrollbar_hover_thumb.clone()
            } else {
                self.horizontal_scrollbar_thumb.clone()
            };
            for col in thumb_start..thumb_end {
                scroll_bar.write_cell(
                    u16::try_from(col).expect("thumb col fits u16"),
                    0,
                    cell.clone(),
                );
            }
            self.horizontal_thumb_start_col = thumb_start;
            self.horizontal_thumb_end_col = thumb_end;
            children.push(SubSurface {
                origin: RelativePoint {
                    row: i32::from(max.height.saturating_sub(1)),
                    col: 0,
                },
                surface: scroll_bar,
                z_index: 0,
            });
        }

        Surface {
            size: ctx.max.size(),
            widget: None,
            cursor: None,
            buffer: Vec::new(),
            children,
        }
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::Mouse(mouse) = event else {
            return;
        };

        if self.is_dragging_vertical_thumb {
            if mouse.kind == mouse::Type::Release && mouse.button == mouse::Button::Left {
                self.is_dragging_vertical_thumb = false;
                ctx.redraw = true;

                let is_over = i64::from(mouse.col)
                    == i64::from(self.last_frame_size.width.saturating_sub(1))
                    && i64::from(mouse.row) >= i64::from(self.vertical_thumb_top_row)
                    && i64::from(mouse.row) < i64::from(self.vertical_thumb_bottom_row);
                if !is_over {
                    self.is_hovering_vertical_thumb = false;
                }
                // Consume so ending the drag does not trigger other handlers.
                return ctx.consume_event();
            }

            if mouse.kind == mouse::Type::Drag {
                ctx.consume_event();
                let new_thumb_top = mouse
                    .row
                    .saturating_sub(i16::from(self.mouse_offset_into_thumb));
                if new_thumb_top <= 0 {
                    self.view.borrow_mut().set_scroll_top(0);
                    return ctx.consume_and_redraw();
                }
                let new_thumb_top_f = f32::from(new_thumb_top);
                let widget_height_f = f32::from(self.last_frame_size.height);
                let total_num_children_f = num::usize_to_f32(self.view.borrow().total_item_count());
                let new_top_child_idx_f = new_thumb_top_f * total_num_children_f / widget_height_f;
                self.view
                    .borrow_mut()
                    .set_scroll_top(num::f32_to_u32(new_top_child_idx_f));
                return ctx.consume_and_redraw();
            }
        }

        if self.is_dragging_horizontal_thumb {
            if mouse.kind == mouse::Type::Release && mouse.button == mouse::Button::Left {
                self.is_dragging_horizontal_thumb = false;
                ctx.redraw = true;

                let is_over = i64::from(mouse.row)
                    == i64::from(self.last_frame_size.height.saturating_sub(1))
                    && i64::from(mouse.col) >= i64::from(self.horizontal_thumb_start_col)
                    && i64::from(mouse.col) < i64::from(self.horizontal_thumb_end_col);
                if !is_over {
                    self.is_hovering_horizontal_thumb = false;
                }
                return ctx.consume_event();
            }

            if mouse.kind == mouse::Type::Drag {
                ctx.consume_event();
                let new_thumb_col_start = mouse
                    .col
                    .saturating_sub(i16::from(self.mouse_offset_into_thumb));
                if new_thumb_col_start <= 0 {
                    self.view.borrow_mut().set_scroll_left(0);
                    return ctx.consume_and_redraw();
                }
                let new_thumb_col_start_f = f32::from(new_thumb_col_start);
                let widget_width_f = f32::from(self.last_frame_size.width);
                let max_content_width_f = num::u32_to_f32(self.last_frame_max_content_width);
                let new_view_col_start_f =
                    new_thumb_col_start_f * max_content_width_f / widget_width_f;
                let new_view_col_start = num::f32_to_u32(new_view_col_start_f.ceil());
                self.view
                    .borrow_mut()
                    .set_scroll_left(new_view_col_start.min(self.last_frame_max_content_width));
                ctx.consume_and_redraw();
            }
        }
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            Event::Mouse(mouse) => {
                let mouse_col: u16 = if mouse.col < 0 {
                    0
                } else {
                    u16::try_from(mouse.col).expect("non-negative col fits u16")
                };
                let mouse_row: u16 = if mouse.row < 0 {
                    0
                } else {
                    u16::try_from(mouse.row).expect("non-negative row fits u16")
                };

                // Vertical thumb hover.
                let is_over_v = mouse_col == self.last_frame_size.width.saturating_sub(1)
                    && u32::from(mouse_row) >= self.vertical_thumb_top_row
                    && u32::from(mouse_row) < self.vertical_thumb_bottom_row;
                if !self.is_hovering_vertical_thumb && is_over_v {
                    self.is_hovering_vertical_thumb = true;
                    ctx.redraw = true;
                } else if self.is_hovering_vertical_thumb && !is_over_v {
                    self.is_hovering_vertical_thumb = false;
                    ctx.redraw = true;
                }
                if is_over_v
                    && mouse.kind == mouse::Type::Press
                    && mouse.button == mouse::Button::Left
                {
                    self.is_dragging_vertical_thumb = true;
                    self.mouse_offset_into_thumb = u8::try_from(
                        u32::from(mouse_row).saturating_sub(self.vertical_thumb_top_row),
                    )
                    .unwrap_or(u8::MAX);
                    return ctx.consume_event();
                }

                // Horizontal thumb hover.
                let is_over_h = mouse_row == self.last_frame_size.height.saturating_sub(1)
                    && u32::from(mouse_col) >= self.horizontal_thumb_start_col
                    && u32::from(mouse_col) < self.horizontal_thumb_end_col;
                if !self.is_hovering_horizontal_thumb && is_over_h {
                    self.is_hovering_horizontal_thumb = true;
                    ctx.redraw = true;
                } else if self.is_hovering_horizontal_thumb && !is_over_h {
                    self.is_hovering_horizontal_thumb = false;
                    ctx.redraw = true;
                }
                if is_over_h
                    && mouse.kind == mouse::Type::Press
                    && mouse.button == mouse::Button::Left
                {
                    self.is_dragging_horizontal_thumb = true;
                    self.mouse_offset_into_thumb = u8::try_from(
                        u32::from(mouse_col).saturating_sub(self.horizontal_thumb_start_col),
                    )
                    .unwrap_or(u8::MAX);
                    ctx.consume_event();
                }
            }
            Event::MouseLeave => self.is_dragging_vertical_thumb = false,
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
    use std::rc::Rc;

    use super::*;
    use crate::gwidth;
    use crate::vxfw::{MaxSize, Point, Source, Text, WidgetRef, widget_eq};

    fn text(s: &str) -> WidgetRef {
        Rc::new(RefCell::new(Text::new(s)))
    }

    /// `ScrollBars` composes with a `ListView` through `ScrollableView`: the
    /// vertical bar draws only when the list overflows the viewport, and a
    /// thumb drag jumps the list's scroll position.
    #[test]
    fn scroll_bars_wraps_a_list_view() {
        use crate::vxfw::ListView;

        // Twenty one-row items in a five-row viewport.
        let items: Vec<WidgetRef> = (0..20).map(|i| text(&i.to_string())).collect();
        let mut lv = ListView::new(Source::Slice(items));
        lv.draw_cursor = false;

        let mut sb = ScrollBars::new(lv);
        sb.draw_horizontal_scrollbar = false;
        let inner = Rc::clone(&sb.view);

        let ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(8),
                height: Some(5),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        };

        // Overflowing content: the inner list plus the vertical bar, with a
        // thumb pinned to the top (5 of 20 items visible, thumb row 0).
        let surface = sb.draw(&ctx);
        assert_eq!(surface.children.len(), 2);
        let bar = surface
            .children
            .iter()
            .find(|child| child.surface.size.width == 1 && child.origin.col == 7)
            .expect("the vertical scroll bar was drawn");
        assert_eq!(bar.surface.read_cell(0, 0).char.grapheme(), "▐");

        // Press on the thumb starts a drag.
        let mut ec = EventContext::new();
        let press = Event::Mouse(mouse::Mouse {
            col: 7,
            row: 0,
            xoffset: 0,
            yoffset: 0,
            button: mouse::Button::Left,
            mods: mouse::Modifiers::empty(),
            kind: mouse::Type::Press,
        });
        sb.handle_event(&mut ec, &press);
        assert!(ec.consume_event, "the press was grabbed by the thumb");

        // Dragging two rows down jumps the list: 2/5 of 20 items = item 8.
        let mut ec = EventContext::new();
        let drag = Event::Mouse(mouse::Mouse {
            col: 7,
            row: 2,
            xoffset: 0,
            yoffset: 0,
            button: mouse::Button::Left,
            mods: mouse::Modifiers::empty(),
            kind: mouse::Type::Drag,
        });
        sb.capture_event(&mut ec, &drag);
        assert!(ec.consume_event, "the drag was intercepted");
        assert_eq!(inner.borrow().scroll_top(), 8);

        // Redraw reconciles: the thumb followed the drag away from the top.
        let surface = sb.draw(&ctx);
        let bar = surface
            .children
            .iter()
            .find(|child| child.surface.size.width == 1 && child.origin.col == 7)
            .expect("the vertical scroll bar is still drawn");
        assert_eq!(bar.surface.read_cell(0, 0).char.grapheme(), " ");

        // Short content: no bar, just the inner list.
        let mut lv = ListView::new(Source::Slice(vec![text("a"), text("b")]));
        lv.draw_cursor = false;
        let mut sb = ScrollBars::new(lv);
        sb.draw_horizontal_scrollbar = false;
        let surface = sb.draw(&ctx);
        assert_eq!(surface.children.len(), 1);
    }

    #[test]
    fn scroll_bars() {
        let mut sv = ScrollView::new(Source::Slice(vec![
            text("abc\n  def\n  ghi"),
            text("def"),
            text("ghi"),
            text("jkl\n mno"),
        ]));
        sv.wheel_scroll = 1;

        let mut scroll_bars = ScrollBars::new(sv);
        scroll_bars.estimated_content_height = Some(7);
        scroll_bars.estimated_content_width = Some(5);

        let ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(3),
                height: Some(4),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        };

        // Both bars and the scroll view.
        let surface = scroll_bars.draw(&ctx);
        assert_eq!(surface.children.len(), 3);

        // Hide only the horizontal scroll bar.
        scroll_bars.draw_horizontal_scrollbar = false;
        let surface = scroll_bars.draw(&ctx);
        assert_eq!(surface.children.len(), 2);

        // Hide only the vertical scroll bar.
        scroll_bars.draw_horizontal_scrollbar = true;
        scroll_bars.draw_vertical_scrollbar = false;
        let surface = scroll_bars.draw(&ctx);
        assert_eq!(surface.children.len(), 2);

        // Hide both scroll bars.
        scroll_bars.draw_horizontal_scrollbar = false;
        let surface = scroll_bars.draw(&ctx);
        assert_eq!(surface.children.len(), 1);

        // Re-enable both bars.
        scroll_bars.draw_horizontal_scrollbar = true;
        scroll_bars.draw_vertical_scrollbar = true;

        // A small estimate still draws the bars when the view knows there is
        // more to render.
        scroll_bars.estimated_content_height = Some(2);
        scroll_bars.estimated_content_width = Some(1);
        let surface = scroll_bars.draw(&ctx);
        assert_eq!(surface.children.len(), 3);

        // The view can tell whether the bars are needed even without estimates.
        scroll_bars.estimated_content_height = None;
        scroll_bars.estimated_content_width = None;
        let surface = scroll_bars.draw(&ctx);
        assert_eq!(surface.children.len(), 3);
    }

    #[test]
    fn vertical_thumb_pinned_to_bottom_is_not_clipped() {
        // The vertical bar surface is one row shorter than the widget when the
        // horizontal bar is enabled. When the view is scrolled to the bottom the
        // thumb is pinned to the bar's end; it must sit flush inside that
        // shorter surface, not one row lower where its bottom cell is clipped
        // away. A clipped thumb loses a row (appears smaller) or, at a one-row
        // thumb, vanishes entirely.
        let items: Vec<WidgetRef> = (0..40).map(|i| text(&i.to_string())).collect();
        let mut sv = ScrollView::new(Source::Slice(items));
        sv.wheel_scroll = 3;

        let mut sb = ScrollBars::new(sv);
        // bar_height^2 / estimate = 25 / 12 rounds to a two-row thumb, so a
        // clip would shrink it to one row and the assertions would catch it.
        sb.estimated_content_height = Some(12);
        let inner = Rc::clone(&sb.view);
        let scroll_bars: WidgetRef = Rc::new(RefCell::new(sb));

        let ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(8),
                height: Some(6),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        };
        let bar_height: u16 = 5; // max.height - horizontal bar row.

        // Drive the inner view to the very bottom, redrawing to reconcile each
        // wheel step into a concrete scroll position.
        let _ = draw_widget(&scroll_bars, &ctx);
        for _ in 0..100 {
            if !inner.borrow().scroll.has_more_vertical {
                break;
            }
            let mut ec = EventContext::new();
            let wheel = Event::Mouse(mouse::Mouse {
                col: 0,
                row: 0,
                xoffset: 0,
                yoffset: 0,
                button: mouse::Button::WheelDown,
                mods: mouse::Modifiers::empty(),
                kind: mouse::Type::Press,
            });
            inner.borrow_mut().handle_event(&mut ec, &wheel);
            let _ = draw_widget(&scroll_bars, &ctx);
        }
        assert!(
            !inner.borrow().scroll.has_more_vertical,
            "the view should be at the bottom"
        );

        let surface = draw_widget(&scroll_bars, &ctx);

        // The vertical bar is the width-1 child at the last column.
        let bar = surface
            .children
            .iter()
            .find(|child| child.surface.size.width == 1 && child.origin.col == 7)
            .expect("the vertical scroll bar was drawn");
        assert_eq!(bar.surface.size.height, bar_height);

        let thumb_glyph = "▐";
        let thumb_rows: Vec<u16> = (0..bar_height)
            .filter(|&row| bar.surface.read_cell(0, row).char.grapheme() == thumb_glyph)
            .collect();

        // Two rows tall (the intended height, no row clipped) and flush against
        // the bar's bottom edge.
        assert_eq!(thumb_rows, vec![bar_height - 2, bar_height - 1]);
    }

    #[test]
    fn scroll_bars_routes_wheel_to_inner_view() {
        // Tall content in a short viewport so the inner view can scroll down.
        let items: Vec<WidgetRef> = (0..20).map(|i| text(&i.to_string())).collect();
        let mut sv = ScrollView::new(Source::Slice(items));
        sv.wheel_scroll = 1;

        let mut sb = ScrollBars::new(sv);
        sb.estimated_content_height = Some(20);
        // Keep a handle to the inner view to inspect its scroll state later.
        let inner = Rc::clone(&sb.view);
        let scroll_bars: WidgetRef = Rc::new(RefCell::new(sb));

        let ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(8),
                height: Some(4),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        };

        // Drawing the composed tree stamps the inner view's surface with its
        // widget identity, so it joins the hit list.
        let surface = draw_widget(&scroll_bars, &ctx);

        // Hit-test a point inside the scroll view (top-left), away from the bars
        // at the far column and bottom row. The deepest hit is the inner
        // ScrollView, not the ScrollBars wrapper.
        let mut hits = Vec::new();
        surface.hit_test(Point { row: 0, col: 0 }, &mut hits);
        let target = hits.pop().expect("the inner scroll view is hit");
        assert!(widget_eq(&target.widget, &to_widget_ref(Rc::clone(&inner))));

        // Deliver a wheel-down to the resolved target, then redraw so the scroll
        // is reconciled into a concrete position.
        let mut ec = EventContext::new();
        let wheel = Event::Mouse(mouse::Mouse {
            col: i16::try_from(target.local.col).expect("local col fits i16"),
            row: i16::try_from(target.local.row).expect("local row fits i16"),
            xoffset: 0,
            yoffset: 0,
            button: mouse::Button::WheelDown,
            mods: mouse::Modifiers::empty(),
            kind: mouse::Type::Press,
        });
        target.widget.borrow_mut().handle_event(&mut ec, &wheel);
        let _ = draw_widget(&scroll_bars, &ctx);

        // The inner view advanced away from the top.
        let sv = inner.borrow();
        assert!(sv.scroll.top > 0 || sv.scroll.vertical_offset > 0);
    }
}
