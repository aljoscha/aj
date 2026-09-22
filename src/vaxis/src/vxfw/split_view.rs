//! [`SplitView`]: two side-by-side panes divided by a draggable vertical rule.

use std::cell::RefCell;
use std::rc::Rc;

use crate::cell::{Cell, Character, Style};
use crate::mouse;
use crate::vxfw::{
    DrawContext, Event, EventContext, MaxSize, RelativePoint, Size, SubSurface, Surface, Widget,
    WidgetRef, draw_widget, to_widget_ref,
};

/// Which pane the width constraints apply to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constrain {
    Lhs,
    Rhs,
}

/// Two panes split by a one-column, mouse-capturing divider.
///
/// Widths exclude the divider. The constrained pane takes [`Self::width`],
/// bounded by its limits and the space reserved for the other pane. When the
/// viewport cannot satisfy both minimums, available space takes precedence.
pub struct SplitView {
    pub lhs: WidgetRef,
    pub rhs: WidgetRef,
    pub constrain: Constrain,
    pub style: Style,
    pub min_width: u16,
    pub max_width: Option<u16>,
    /// Space to reserve for the unconstrained pane while resizing.
    pub min_other_width: u16,
    divider: Rc<RefCell<Divider>>,
}

impl SplitView {
    /// Build a left-constrained split. `width` excludes its one-column divider.
    pub fn new(lhs: WidgetRef, rhs: WidgetRef, width: u16) -> Self {
        Self {
            lhs,
            rhs,
            constrain: Constrain::Lhs,
            style: Style::default(),
            min_width: 0,
            max_width: None,
            min_other_width: 0,
            divider: Rc::new(RefCell::new(Divider {
                width,
                drawn_width: width,
                constrain: Constrain::Lhs,
                min: 0,
                max: 0,
                height: 0,
                style: Style::default(),
                dragging: false,
                hovered: false,
            })),
        }
    }

    /// The constrained pane's width, updated by dragging and clamped at draw.
    pub fn width(&self) -> u16 {
        self.divider.borrow().width
    }

    /// Set the constrained pane's width. The next layout applies its limits.
    pub fn set_width(&mut self, width: u16) {
        self.divider.borrow_mut().width = width;
    }

    /// Whether the divider owns an active resize gesture.
    pub fn is_dragging(&self) -> bool {
        self.divider.borrow().dragging
    }
}

impl Widget for SplitView {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let size = ctx.max.size();
        let width = {
            let mut divider = self.divider.borrow_mut();
            divider.max = size
                .width
                .saturating_sub(1)
                .saturating_sub(self.min_other_width)
                .min(self.max_width.unwrap_or(u16::MAX));
            divider.min = self.min_width.min(divider.max);
            divider.width = divider.width.clamp(divider.min, divider.max);
            divider.drawn_width = divider.width;
            divider.constrain = self.constrain;
            divider.style = self.style;
            divider.width
        };
        let other = size.width.saturating_sub(width).saturating_sub(1);
        let (lhs_width, rhs_width) = match self.constrain {
            Constrain::Lhs => (width, other),
            Constrain::Rhs => (other, width),
        };
        let mut children = Vec::with_capacity(3);
        for (widget, col, width) in [
            (Rc::clone(&self.lhs), 0, lhs_width),
            (Rc::clone(&self.rhs), lhs_width.saturating_add(1), rhs_width),
        ] {
            if width == 0 || size.height == 0 {
                continue;
            }
            let pane = Size {
                width,
                height: size.height,
            };
            children.push(SubSurface {
                origin: RelativePoint {
                    row: 0,
                    col: i32::from(col),
                },
                surface: draw_widget(
                    &widget,
                    &ctx.with_constraints(pane, MaxSize::from_size(pane)),
                ),
                z_index: 0,
            });
        }
        if size.width > 0 && size.height > 0 {
            let divider_size = Size {
                width: 1,
                height: size.height,
            };
            children.push(SubSurface {
                origin: RelativePoint {
                    row: 0,
                    col: i32::from(lhs_width),
                },
                surface: draw_widget(
                    &to_widget_ref(Rc::clone(&self.divider)),
                    &ctx.with_constraints(divider_size, MaxSize::from_size(divider_size)),
                ),
                z_index: 0,
            });
        }
        Surface::with_children(size, children)
    }
}

/// A distinct hit target keeps pane clicks and overlay input out of the resize
/// gesture. Its width is the split's single source of truth, not a callback
/// waiting for an application to reconcile another copy.
struct Divider {
    width: u16,
    /// Mouse coordinates refer to the last painted divider, even if several
    /// drag reports arrive before the next frame.
    drawn_width: u16,
    constrain: Constrain,
    min: u16,
    max: u16,
    height: u16,
    style: Style,
    dragging: bool,
    hovered: bool,
}

impl Divider {
    fn active(&self) -> bool {
        self.dragging || self.hovered
    }

    fn refresh(&self, ctx: &mut EventContext, was_active: bool) {
        if was_active != self.active() {
            ctx.set_mouse_shape(if self.active() {
                mouse::Shape::EwResize
            } else {
                mouse::Shape::Default
            });
            ctx.redraw = true;
        }
    }

    fn resize(&mut self, col: i16, ctx: &mut EventContext) {
        let width = match self.constrain {
            Constrain::Lhs => i32::from(self.drawn_width) + i32::from(col),
            Constrain::Rhs => i32::from(self.drawn_width) - i32::from(col),
        };
        let width = u16::try_from(width.clamp(i32::from(self.min), i32::from(self.max)))
            .expect("width clamped to the pane limits");
        ctx.redraw |= self.width != width;
        self.width = width;
    }
}

impl Widget for Divider {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let size = ctx.max.size();
        self.height = size.height;
        let mut surface = Surface::with_size(size);
        for row in 0..size.height {
            surface.write_cell(
                0,
                row,
                Cell {
                    char: Character::new(if self.active() { "┃" } else { "│" }, 1),
                    style: self.style,
                    ..Cell::default()
                },
            );
        }
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        // A leaf reaches this phase only when another surface covers it.
        if matches!(event, Event::Mouse(_)) {
            let was_active = self.active();
            self.hovered = false;
            self.refresh(ctx, was_active);
        }
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let was_active = self.active();
        match event {
            Event::Mouse(m) if !m.button.is_wheel() => {
                if !self.dragging && matches!(m.kind, mouse::Type::Drag | mouse::Type::Release) {
                    return;
                }
                let in_height = m.row >= 0 && i32::from(m.row) < i32::from(self.height);
                self.hovered = m.col == 0 && in_height;
                if m.kind == mouse::Type::Press && m.button == mouse::Button::Left {
                    self.dragging = true;
                    ctx.capture_mouse();
                } else if self.dragging && m.button == mouse::Button::Left {
                    if matches!(m.kind, mouse::Type::Drag | mouse::Type::Release) {
                        self.resize(m.col, ctx);
                        ctx.consume_event();
                    }
                    if m.kind == mouse::Type::Release {
                        self.dragging = false;
                        // Release may move the divider before a repaint. Hover
                        // follows its resulting position, including clamping.
                        let shift = i32::from(self.width) - i32::from(self.drawn_width);
                        let col = match self.constrain {
                            Constrain::Lhs => shift,
                            Constrain::Rhs => -shift,
                        };
                        self.hovered = i32::from(m.col) == col && in_height;
                    }
                }
            }
            Event::MouseLeave => self.hovered = false,
            Event::MouseCaptureLost | Event::FocusOut => {
                self.dragging = false;
                self.hovered = false;
            }
            _ => {}
        }
        self.refresh(ctx, was_active);
        // Pane hover handlers can change the cursor while capture is active.
        // Reassert it after dispatch and when layout moves the divider under it.
        let restore_cursor = match event {
            Event::Mouse(m) => matches!(m.kind, mouse::Type::Drag | mouse::Type::Release),
            Event::MouseEnter => true,
            _ => false,
        };
        if self.active() && restore_cursor {
            ctx.set_mouse_shape(mouse::Shape::EwResize);
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}
