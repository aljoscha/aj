//! [`ScrollBars`]: wraps a scrollable view and draws draggable scroll bars.
//!
//! The wrapped view is anything implementing [`ScrollableView`], by default a
//! [`ScrollView`]. This is the only widget that overrides
//! [`capture_event`](Widget::capture_event): while a thumb is being dragged it
//! intercepts the drag and release in the capturing phase, before they reach
//! the inner content, and translates the thumb position into a scroll position
//! via the view's [`ScrollableView`] scroll accessors.
//!
//! # Identity and event routing
//!
//! [`draw`](Widget::draw) stamps two identities. It stamps its own returned
//! surface with the bars' identity (via the `self_ref` handle) so the event
//! bus finds the bars in the hit path and routes thumb hover and drag to them.
//! It separately stamps the inner view's surface (via [`draw_widget`]) so wheel
//! and key events reach the view. A consumer embeds the bars surface directly,
//! so both identities travel with it and mouse events route with no forwarding
//! by the consumer.
//!
//! NOTE: A widget cannot name its own `Rc` from inside `draw`, so it cannot put
//! its own identity on the surface it builds there. The bars work around this
//! by carrying a `Weak` self-handle minted by `Rc::new_cyclic` in
//! [`new`](ScrollBars::new) and upgrading it to stamp the surface. The inner
//! view has no such trouble because `draw_widget` assigns its identity from the
//! outside.
//!
//! The bars are sized with floating-point proportions of an estimated content
//! extent. When no estimate is given we fall back to the number and width of the
//! children the [`ScrollView`] actually rendered, which is less stable across
//! frames but needs no caller input.
//!
//! A view may expose a measured line-based geometry (see
//! [`ScrollableView::content_extent`]). When it does, the vertical thumb is
//! sized and placed in that line space instead, which tracks real content
//! height for items of widely differing height. The estimate/item-count path
//! stays the fallback for views without a geometry.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

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
/// well within `f32`'s exact range, and the float math uses proportional
/// thumb sizing.
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

    /// Total content extent in the line-based space the thumb uses, measured
    /// where items have been laid out and estimated elsewhere. `None` when the
    /// view keeps no such geometry (the bars fall back to item-count sizing).
    fn content_extent(&self) -> Option<u32> {
        None
    }
    /// Line offset of the viewport top in the `content_extent` space, as of the
    /// last draw. `None` when no geometry.
    fn viewport_top_line(&self) -> Option<u32> {
        None
    }
    /// Item index to anchor the top at so the viewport top lands at `line`, for
    /// drag-to-jump in the geometry's line space. `None` when no geometry.
    fn item_at_line(&self, _line: u32) -> Option<u32> {
        None
    }
}

/// A scrollable view with draggable, hoverable scroll bars.
///
/// The wrapped view is held behind an `Rc<RefCell<V>>` so it has a stable
/// widget identity. [`draw`](Widget::draw) stamps the inner view's surface
/// with that identity via [`draw_widget`] and appends it as a child, so the
/// event bus hit-tests and routes wheel and key events to the inner view. It
/// also stamps its own surface with the bars' identity (via the `self_ref`
/// handle) so the bus routes thumb hover and drag to the bars, whose
/// interaction lives in this widget's [`handle_event`](Widget::handle_event)
/// and [`capture_event`](Widget::capture_event) and reaches into the view
/// through [`ScrollableView`].
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
    /// Handle `draw` stamps onto its own surface so the event bus routes thumb
    /// hover and drag to this widget, the way an externally assigned identity
    /// routes events to the inner view.
    self_ref: Weak<RefCell<ScrollBars<V>>>,
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
    ///
    /// Returns the shared `Rc<RefCell<Self>>` handle. Making that the only way
    /// to construct a `ScrollBars` guarantees the bars are always routable: an
    /// unroutable one cannot exist, because `draw` stamps its surface with the
    /// `Weak` self-handle minted here.
    pub fn new(view: V) -> Rc<RefCell<ScrollBars<V>>> {
        Rc::new_cyclic(|weak| {
            RefCell::new(ScrollBars {
                self_ref: Weak::clone(weak),
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
            })
        })
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
            // NOTE: `upgrade()` only bumps the `Rc` strong count, it does not
            // borrow the `RefCell`, so stamping our own identity from inside
            // `draw` (which runs behind a `borrow_mut`) is panic-free.
            return Surface {
                size: ctx.max.size(),
                widget: self.self_ref.upgrade().map(to_widget_ref),
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
        let (scroll_top, has_more_vertical, content_extent, viewport_top_line) = {
            let view = self.view.borrow();
            (
                view.scroll_top(),
                view.has_more_below(),
                view.content_extent(),
                view.viewport_top_line(),
            )
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

            // When the view exposes a measured content extent we size and place
            // the thumb in that line space, which tracks real content height far
            // better than the item count for entries of wildly differing height.
            // Otherwise fall back to the estimate/item-count sizing below.
            let geometry = match (content_extent, viewport_top_line) {
                (Some(total), Some(top_line)) if total > 0 => Some((total, top_line)),
                _ => None,
            };

            let (thumb_height, thumb_top): (u16, u32) = if let Some((total, top_line)) = geometry {
                let content_extent_f = num::u32_to_f32(total);
                let thumb_height_f = (widget_height_f * widget_height_f / content_extent_f).round();
                let thumb_height = num::f32_to_u16(thumb_height_f.max(1.0));
                let thumb_top = if scroll_top == 0 {
                    0
                } else if has_more_vertical {
                    let top_line_f = num::u32_to_f32(top_line);
                    let thumb_top_f = (widget_height_f * top_line_f / content_extent_f).round();
                    num::f32_to_u32(thumb_top_f)
                } else {
                    u32::from(bar_height.saturating_sub(thumb_height))
                };
                // Never let the thumb overrun the bar's bottom edge.
                let thumb_top = thumb_top.min(u32::from(bar_height.saturating_sub(thumb_height)));
                (thumb_height, thumb_top)
            } else {
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
                (thumb_height, thumb_top)
            };

            // NOTE: the bar surfaces carry no identity on purpose. The outer
            // surface is the hit target, so `handle_event` reads the mouse in
            // the outer widget frame and its `col == width - 1` thumb test
            // holds. Stamping this width-1 child would rebase a thumb click to
            // its own col 0 and defeat that test.
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
            widget: self.self_ref.upgrade().map(to_widget_ref),
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
                // With a geometry the thumb sits in line space, so map the
                // thumb-top row to a target line, then to the item index whose
                // top lands there. Read the geometry, drop the borrow, then
                // write, so we never hold a borrow across the borrow_mut.
                let content_extent = self.view.borrow().content_extent();
                if let Some(total) = content_extent.filter(|&t| t > 0) {
                    let total_f = num::u32_to_f32(total);
                    let target_line_f = (new_thumb_top_f * total_f / widget_height_f).round();
                    let target_line = num::f32_to_u32(target_line_f);
                    let idx = self.view.borrow().item_at_line(target_line);
                    if let Some(idx) = idx {
                        self.view.borrow_mut().set_scroll_top(idx);
                    }
                    return ctx.consume_and_redraw();
                }
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
    use crate::vxfw::{HitResult, MaxSize, Phase, Point, Source, Text, WidgetRef, widget_eq};

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

        let sb = ScrollBars::new(lv);
        sb.borrow_mut().draw_horizontal_scrollbar = false;
        let inner = Rc::clone(&sb.borrow().view);

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
        let surface = sb.borrow_mut().draw(&ctx);
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
        sb.borrow_mut().handle_event(&mut ec, &press);
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
        sb.borrow_mut().capture_event(&mut ec, &drag);
        assert!(ec.consume_event, "the drag was intercepted");
        assert_eq!(inner.borrow().scroll_top(), 8);

        // Redraw reconciles: the thumb followed the drag away from the top.
        let surface = sb.borrow_mut().draw(&ctx);
        let bar = surface
            .children
            .iter()
            .find(|child| child.surface.size.width == 1 && child.origin.col == 7)
            .expect("the vertical scroll bar is still drawn");
        assert_eq!(bar.surface.read_cell(0, 0).char.grapheme(), " ");

        // Short content: no bar, just the inner list.
        let mut lv = ListView::new(Source::Slice(vec![text("a"), text("b")]));
        lv.draw_cursor = false;
        let sb = ScrollBars::new(lv);
        sb.borrow_mut().draw_horizontal_scrollbar = false;
        let surface = sb.borrow_mut().draw(&ctx);
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

        let scroll_bars = ScrollBars::new(sv);
        scroll_bars.borrow_mut().estimated_content_height = Some(7);
        scroll_bars.borrow_mut().estimated_content_width = Some(5);

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
        let surface = scroll_bars.borrow_mut().draw(&ctx);
        assert_eq!(surface.children.len(), 3);

        // Hide only the horizontal scroll bar.
        scroll_bars.borrow_mut().draw_horizontal_scrollbar = false;
        let surface = scroll_bars.borrow_mut().draw(&ctx);
        assert_eq!(surface.children.len(), 2);

        // Hide only the vertical scroll bar.
        scroll_bars.borrow_mut().draw_horizontal_scrollbar = true;
        scroll_bars.borrow_mut().draw_vertical_scrollbar = false;
        let surface = scroll_bars.borrow_mut().draw(&ctx);
        assert_eq!(surface.children.len(), 2);

        // Hide both scroll bars.
        scroll_bars.borrow_mut().draw_horizontal_scrollbar = false;
        let surface = scroll_bars.borrow_mut().draw(&ctx);
        assert_eq!(surface.children.len(), 1);

        // Re-enable both bars.
        scroll_bars.borrow_mut().draw_horizontal_scrollbar = true;
        scroll_bars.borrow_mut().draw_vertical_scrollbar = true;

        // A small estimate still draws the bars when the view knows there is
        // more to render.
        scroll_bars.borrow_mut().estimated_content_height = Some(2);
        scroll_bars.borrow_mut().estimated_content_width = Some(1);
        let surface = scroll_bars.borrow_mut().draw(&ctx);
        assert_eq!(surface.children.len(), 3);

        // The view can tell whether the bars are needed even without estimates.
        scroll_bars.borrow_mut().estimated_content_height = None;
        scroll_bars.borrow_mut().estimated_content_width = None;
        let surface = scroll_bars.borrow_mut().draw(&ctx);
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

        let sb = ScrollBars::new(sv);
        // bar_height^2 / estimate = 25 / 12 rounds to a two-row thumb, so a
        // clip would shrink it to one row and the assertions would catch it.
        sb.borrow_mut().estimated_content_height = Some(12);
        let inner = Rc::clone(&sb.borrow().view);
        let scroll_bars: WidgetRef = to_widget_ref(sb);

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

        let sb = ScrollBars::new(sv);
        sb.borrow_mut().estimated_content_height = Some(20);
        // Keep a handle to the inner view to inspect its scroll state later.
        let inner = Rc::clone(&sb.borrow().view);
        let scroll_bars: WidgetRef = to_widget_ref(sb);

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

    /// A geometry-backed `ListView` with tall early items and short later ones
    /// places the vertical thumb by measured line offset, not item index, so
    /// the thumb top differs from the pure item-count position.
    #[test]
    fn scroll_bars_vertical_thumb_uses_list_geometry() {
        use crate::vxfw::ListView;

        // Three tall items (10 rows each) then a long run of one-row items. The
        // geometry sees a top-heavy content extent that the item-count formula,
        // which weights every item the same, cannot.
        let mut items: Vec<WidgetRef> = Vec::new();
        for _ in 0..3 {
            items.push(text("a\nb\nc\nd\ne\nf\ng\nh\ni\nj"));
        }
        for _ in 0..18 {
            items.push(text("x"));
        }
        let mut lv = ListView::new(Source::Slice(items));
        lv.draw_cursor = false;
        let sb = ScrollBars::new(lv);
        sb.borrow_mut().draw_horizontal_scrollbar = false;
        let inner = Rc::clone(&sb.borrow().view);

        let ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(16),
                height: Some(10),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        };

        // First draw establishes item_count and seeds the geometry.
        sb.borrow_mut().draw(&ctx);
        // Scroll past the three tall items so the top lands on the short run.
        inner.borrow_mut().scroll_lines(30);
        let surface = sb.borrow_mut().draw(&ctx);

        let bar_height: u16 = 10; // No horizontal bar reserved.
        let widget_h_f = f32::from(bar_height);

        let (extent, top_line, scroll_top, total_items) = {
            let v = inner.borrow();
            (
                v.content_extent().expect("geometry-backed extent"),
                v.viewport_top_line().expect("geometry-backed top line"),
                v.scroll_top(),
                v.total_item_count(),
            )
        };
        assert!(scroll_top > 0, "the scroll left the top");

        // The geometry places the thumb by line-space fraction, rounded.
        let extent_f = num::u32_to_f32(extent);
        let thumb_height = num::f32_to_u16((widget_h_f * widget_h_f / extent_f).round().max(1.0));
        let geom_top = num::f32_to_u32((widget_h_f * num::u32_to_f32(top_line) / extent_f).round())
            .min(u32::from(bar_height.saturating_sub(thumb_height)));

        // The item-count formula places it by index-space fraction, truncated.
        let count_top = num::f32_to_u32(
            widget_h_f * num::u32_to_f32(scroll_top) / num::usize_to_f32(total_items),
        );

        assert_ne!(
            geom_top, count_top,
            "the tall early items move the thumb off the item-count position"
        );

        let bar = surface
            .children
            .iter()
            .find(|c| c.surface.size.width == 1 && c.origin.col == 15)
            .expect("the vertical bar was drawn");
        let first_thumb_row = (0..bar_height)
            .find(|&r| bar.surface.read_cell(0, r).char.grapheme() == "▐")
            .expect("the thumb glyph is present");
        assert_eq!(u32::from(first_thumb_row), geom_top);
    }

    /// With all-equal item heights the geometry's line space matches the item
    /// index space, so the thumb lands where the item-count formula would put
    /// it. No regression for uniform lists.
    #[test]
    fn scroll_bars_vertical_thumb_matches_item_count_for_uniform_list() {
        use crate::vxfw::ListView;

        let items: Vec<WidgetRef> = (0..20).map(|i| text(&i.to_string())).collect();
        let mut lv = ListView::new(Source::Slice(items));
        lv.draw_cursor = false;
        let sb = ScrollBars::new(lv);
        sb.borrow_mut().draw_horizontal_scrollbar = false;
        let inner = Rc::clone(&sb.borrow().view);

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

        sb.borrow_mut().draw(&ctx);
        inner.borrow_mut().scroll_lines(8);
        let surface = sb.borrow_mut().draw(&ctx);

        let bar_height: u16 = 5;
        let widget_h_f = f32::from(bar_height);
        let (extent, top_line, scroll_top, total_items) = {
            let v = inner.borrow();
            (
                v.content_extent().expect("geometry-backed extent"),
                v.viewport_top_line().expect("geometry-backed top line"),
                v.scroll_top(),
                v.total_item_count(),
            )
        };
        // One-row items: the line space equals the index space.
        assert_eq!(extent, u32::try_from(total_items).expect("count fits u32"));
        assert_eq!(u64::from(top_line), u64::from(scroll_top));

        let extent_f = num::u32_to_f32(extent);
        let thumb_height = num::f32_to_u16((widget_h_f * widget_h_f / extent_f).round().max(1.0));
        let geom_top = num::f32_to_u32((widget_h_f * num::u32_to_f32(top_line) / extent_f).round())
            .min(u32::from(bar_height.saturating_sub(thumb_height)));
        let count_top = num::f32_to_u32(
            widget_h_f * num::u32_to_f32(scroll_top) / num::usize_to_f32(total_items),
        );
        assert_eq!(
            geom_top, count_top,
            "uniform heights keep the geometry thumb at the item-count position"
        );

        let bar = surface
            .children
            .iter()
            .find(|c| c.surface.size.width == 1 && c.origin.col == 7)
            .expect("the vertical bar was drawn");
        let first_thumb_row = (0..bar_height)
            .find(|&r| bar.surface.read_cell(0, r).char.grapheme() == "▐")
            .expect("the thumb glyph is present");
        assert_eq!(u32::from(first_thumb_row), geom_top);
    }

    /// Drawing the bars directly (not through [`draw_widget`]) still yields a
    /// routable surface: the self-stamp puts the bars' own identity on it, so a
    /// consumer that embeds the surface hands the bus a hit path that reaches
    /// the bars.
    #[test]
    fn scroll_bars_draw_self_stamps_identity() {
        use crate::vxfw::ListView;

        let items: Vec<WidgetRef> = (0..20).map(|i| text(&i.to_string())).collect();
        let mut lv = ListView::new(Source::Slice(items));
        lv.draw_cursor = false;
        let bars = ScrollBars::new(lv);
        bars.borrow_mut().draw_horizontal_scrollbar = false;

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

        // Draw directly, the way the by-value consumers embed the surface.
        let surface = bars.borrow_mut().draw(&ctx);
        let stamped = surface
            .widget
            .as_ref()
            .expect("the bars stamp their own surface");
        assert!(
            widget_eq(stamped, &to_widget_ref(Rc::clone(&bars))),
            "the stamped identity is the bars handle"
        );
    }

    /// The bars-less early return self-stamps too: a `ScrollBars` with both
    /// bars disabled still hands back a routable surface, matching the main
    /// return path. A [`FilterableSelect`] whose list fits hits this path every
    /// frame, so the second stamp site must not regress to `None`.
    ///
    /// [`FilterableSelect`]: crate::vxfw::FilterableSelect
    #[test]
    fn scroll_bars_no_bars_draw_self_stamps_identity() {
        use crate::vxfw::ListView;

        let items: Vec<WidgetRef> = (0..3).map(|i| text(&i.to_string())).collect();
        let mut lv = ListView::new(Source::Slice(items));
        lv.draw_cursor = false;
        let bars = ScrollBars::new(lv);
        {
            let mut b = bars.borrow_mut();
            b.draw_horizontal_scrollbar = false;
            b.draw_vertical_scrollbar = false;
        }

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

        let surface = bars.borrow_mut().draw(&ctx);
        let stamped = surface
            .widget
            .as_ref()
            .expect("the bars-less surface still stamps its own identity");
        assert!(
            widget_eq(stamped, &to_widget_ref(Rc::clone(&bars))),
            "the stamped identity is the bars handle"
        );
    }

    /// A self-stamped bars surface is routable through the bus with no consumer
    /// forwarding: a mouse Press on the thumb followed by a Drag, dispatched by
    /// the same capture/target/bubble walk the App runs, scrolls the wrapped
    /// view.
    #[test]
    fn scroll_bars_press_then_drag_scrolls_inner_view() {
        use crate::vxfw::ListView;

        let items: Vec<WidgetRef> = (0..20).map(|i| text(&i.to_string())).collect();
        let mut lv = ListView::new(Source::Slice(items));
        lv.draw_cursor = false;
        let bars = ScrollBars::new(lv);
        bars.borrow_mut().draw_horizontal_scrollbar = false;
        let inner = Rc::clone(&bars.borrow().view);

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

        // Draw directly (no `draw_widget` wrapper): the self-stamp is all the
        // routing needs.
        let surface = bars.borrow_mut().draw(&ctx);

        // Press on the thumb (last column, top row). It hit-tests to the bars,
        // whose at-target handler starts the drag.
        let press = mouse::Mouse {
            col: 7,
            row: 0,
            xoffset: 0,
            yoffset: 0,
            button: mouse::Button::Left,
            mods: mouse::Modifiers::empty(),
            kind: mouse::Type::Press,
        };
        let ec = dispatch_mouse(&surface, press);
        assert!(ec.consume_event, "the press was grabbed by the thumb");
        assert!(
            bars.borrow().is_dragging_vertical_thumb,
            "the press started a thumb drag"
        );

        // Drag two rows down over the content area. It hit-tests to the inner
        // view with the bars as ancestor, so the bars' capture-phase handler
        // intercepts it and jumps the list (2/5 of 20 items = item 8).
        let drag = mouse::Mouse {
            col: 0,
            row: 2,
            xoffset: 0,
            yoffset: 0,
            button: mouse::Button::Left,
            mods: mouse::Modifiers::empty(),
            kind: mouse::Type::Drag,
        };
        let ec = dispatch_mouse(&surface, drag);
        assert!(ec.consume_event, "the drag was intercepted by the bars");
        assert_eq!(
            inner.borrow().scroll_top(),
            8,
            "the drag scrolled the wrapped view through bus routing"
        );
    }

    /// Dispatches `mouse` through `surface` the way the App's mouse handler
    /// does: hit-test, then walk the capture phase (root to target-exclusive),
    /// the target, and the bubble phase, stopping on consume. This is the
    /// routing the self-stamp enables, exercised without a running App. It
    /// omits the enter/leave diffing and command draining the App also does,
    /// neither of which affects the routing under test.
    fn dispatch_mouse(surface: &Surface, mouse: mouse::Mouse) -> EventContext {
        let mut ec = EventContext::new();
        let point = Point {
            col: u16::try_from(mouse.col).expect("col fits u16"),
            row: u16::try_from(mouse.row).expect("row fits u16"),
        };
        let mut hits: Vec<HitResult> = Vec::new();
        surface.hit_test(point, &mut hits);
        let Some(target) = hits.pop() else {
            return ec;
        };

        ec.phase = Phase::Capturing;
        for item in &hits {
            let event = local_mouse_event(mouse, item.local);
            item.widget.borrow_mut().capture_event(&mut ec, &event);
            if ec.consume_event {
                return ec;
            }
        }

        ec.phase = Phase::AtTarget;
        {
            let event = local_mouse_event(mouse, target.local);
            target.widget.borrow_mut().handle_event(&mut ec, &event);
            if ec.consume_event {
                return ec;
            }
        }

        ec.phase = Phase::Bubbling;
        while let Some(item) = hits.pop() {
            let event = local_mouse_event(mouse, item.local);
            item.widget.borrow_mut().handle_event(&mut ec, &event);
            if ec.consume_event {
                return ec;
            }
        }
        ec
    }

    /// Rewrites `mouse`'s coordinates into a widget's local frame, the way the
    /// App translates a report before delivering it along the hit path.
    fn local_mouse_event(mouse: mouse::Mouse, local: Point) -> Event {
        let mut m = mouse;
        m.col = i16::try_from(local.col).expect("local col fits i16");
        m.row = i16::try_from(local.row).expect("local row fits i16");
        Event::Mouse(m)
    }
}
