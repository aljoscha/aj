//! [`ScrollBars`]: wraps a [`ScrollView`] and draws draggable scroll bars.
//!
//! This is the only widget that overrides [`capture_event`](Widget::capture_event):
//! while a thumb is being dragged it intercepts the drag and release in the
//! capturing phase, before they reach the inner content, and translates the
//! thumb position into a scroll position by reaching directly into the
//! [`ScrollView`]'s `scroll` state.
//!
//! The bars are sized with floating-point proportions of an estimated content
//! extent. When no estimate is given we fall back to the number and width of the
//! children the [`ScrollView`] actually rendered, which is less stable across
//! frames but needs no caller input.

use crate::cell::{Cell, Character, Color, Style};
use crate::mouse;
use crate::vxfw::scroll_view::ScrollView;
use crate::vxfw::{
    DrawContext, Event, EventContext, RelativePoint, Size, Source, SubSurface, Surface, Widget,
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

/// A [`ScrollView`] with draggable, hoverable scroll bars.
///
/// The wrapped view is held by value (`scroll_view`); the bars reach into its
/// `scroll` state to read and drive the scroll position.
///
/// NOTE: Upstream stamps the inner `ScrollView`'s surface with its own widget
/// identity so the event bus routes wheel and key events to it. Held by value
/// here, the inner view has no `WidgetRef` identity, so those events are not
/// bus-routed to it. The drag interaction still works because `ScrollBars`
/// reaches into `scroll_view.scroll` directly.
pub struct ScrollBars {
    /// The wrapped scroll view. The bars are drawn for this view.
    pub scroll_view: ScrollView,
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

impl ScrollBars {
    /// Wraps `scroll_view` with both bars enabled and the default thumb cells.
    pub fn new(scroll_view: ScrollView) -> ScrollBars {
        ScrollBars {
            scroll_view,
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

/// Total number of items in the scroll view: its `item_count`, a slice's
/// length, or a count of how many indices the builder yields.
fn total_item_count(sv: &ScrollView) -> usize {
    if let Some(c) = sv.item_count {
        return usize::try_from(c).expect("item count fits usize");
    }
    match &sv.children {
        Source::Slice(slice) => slice.len(),
        Source::Builder(builder) => {
            let cursor = usize::try_from(sv.cursor).expect("cursor fits usize");
            let mut counter = 0;
            while builder.item(counter, cursor).is_some() {
                counter += 1;
            }
            counter
        }
    }
}

impl Widget for ScrollBars {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let mut children: Vec<SubSurface> = Vec::new();

        // No bars: draw the scroll view directly.
        if !self.draw_vertical_scrollbar && !self.draw_horizontal_scrollbar {
            children.push(SubSurface {
                origin: RelativePoint { row: 0, col: 0 },
                surface: self.scroll_view.draw(ctx),
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
        let scroll_view_surface = self.scroll_view.draw(
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

        // Vertical scroll bar.
        if self.draw_vertical_scrollbar
            && !(self.scroll_view.scroll.top == 0 && !self.scroll_view.scroll.has_more_vertical)
        {
            let widget_height_f = f32::from(scroll_view_height);
            let total_num_children_f = num::usize_to_f32(total_item_count(&self.scroll_view));

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

            let thumb_top: u32 = if self.scroll_view.scroll.top == 0 {
                0
            } else if self.scroll_view.scroll.has_more_vertical {
                let top_child_idx_f = num::u32_to_f32(self.scroll_view.scroll.top);
                let thumb_top_f = widget_height_f * top_child_idx_f / total_num_children_f;
                num::f32_to_u32(thumb_top_f)
            } else {
                u32::from(max.height.saturating_sub(thumb_height))
            };

            let mut scroll_bar = Surface::with_size(Size {
                width: 1,
                height: max
                    .height
                    .saturating_sub(u16::from(self.draw_horizontal_scrollbar)),
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
        let should_draw_horizontal =
            self.scroll_view.scroll.left > 0 || self.scroll_view.scroll.has_more_horizontal;
        if self.draw_horizontal_scrollbar && should_draw_horizontal {
            let widget_width_f = f32::from(max.width);

            let max_content_width: u32 = self.estimated_content_width.unwrap_or(max_rendered_width);

            let max_content_width_f =
                if self.scroll_view.scroll.left + u32::from(max.width) > max_content_width {
                    // Overscrolled (e.g. the content changed): widen the content
                    // so the thumb does not vanish.
                    num::u32_to_f32(self.scroll_view.scroll.left + u32::from(max.width))
                } else {
                    num::u32_to_f32(max_content_width)
                };
            self.last_frame_max_content_width = max_content_width;

            let thumb_width_f = widget_width_f * widget_width_f / max_content_width_f;
            let thumb_width = num::f32_to_u32(thumb_width_f.max(1.0));

            let view_start_col_f = num::u32_to_f32(self.scroll_view.scroll.left);
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
                    self.scroll_view.scroll.top = 0;
                    return ctx.consume_and_redraw();
                }
                let new_thumb_top_f = f32::from(new_thumb_top);
                let widget_height_f = f32::from(self.last_frame_size.height);
                let total_num_children_f = num::usize_to_f32(total_item_count(&self.scroll_view));
                let new_top_child_idx_f = new_thumb_top_f * total_num_children_f / widget_height_f;
                self.scroll_view.scroll.top = num::f32_to_u32(new_top_child_idx_f);
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
                    self.scroll_view.scroll.left = 0;
                    return ctx.consume_and_redraw();
                }
                let new_thumb_col_start_f = f32::from(new_thumb_col_start);
                let widget_width_f = f32::from(self.last_frame_size.width);
                let max_content_width_f = num::u32_to_f32(self.last_frame_max_content_width);
                let new_view_col_start_f =
                    new_thumb_col_start_f * max_content_width_f / widget_width_f;
                let new_view_col_start = num::f32_to_u32(new_view_col_start_f.ceil());
                self.scroll_view.scroll.left =
                    new_view_col_start.min(self.last_frame_max_content_width);
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
    use crate::vxfw::{MaxSize, Text, WidgetRef};

    fn text(s: &str) -> WidgetRef {
        Rc::new(RefCell::new(Text::new(s)))
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
}
