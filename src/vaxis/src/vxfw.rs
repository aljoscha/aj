//! The retained-mode widget framework: the `Widget` trait, the layout
//! `Surface` tree, the event/command bus, and the `App` runtime.
//!
//! # Widget identity and the draw/event split
//!
//! A widget is owned behind a [`WidgetRef`] (`Rc<RefCell<dyn Widget>>`).
//! Identity is pointer identity over the `Rc`: [`widget_eq`] is the framework's
//! `==` for widgets, used by focus tracking, mouse enter/leave diffing, and
//! cursor rendering. This replaces upstream's pointer-identity comparison over
//! the hand-rolled widget vtable.
//!
//! [`Widget::draw`] takes `&mut self`. Stateful widgets (a list view tracking
//! scroll offset, a spinner advancing a frame) mutate during draw, matching
//! upstream where the draw function casts its userdata to a mutable pointer.
//! Allocation aborts the process on OOM in Rust rather than returning an error,
//! so draw is infallible.
//!
//! A widget cannot reference its own `Rc` from inside `draw`, so a widget never
//! stamps `Surface::widget` itself. Always build child (and root) surfaces via
//! the free [`draw_widget`] helper, which calls the widget's `draw` and then
//! stamps the returned surface with the widget's `Rc`. Composite widgets call
//! `draw_widget(&self.child, &ctx.with_constraints(..))`, and the App calls it
//! for the root. A `Surface` therefore carries `widget: Option<WidgetRef>`: it
//! is `None` while a widget assembles it and `Some` once `draw_widget` stamps
//! it.
//!
//! # Event propagation
//!
//! Events travel in three phases (see [`Phase`]): a capturing phase from the
//! root down to the target, an at-target phase, and a bubbling phase from the
//! target back up to the root. A handler stops propagation by setting
//! [`EventContext::consume_event`]. The App drives the walk (see the `app`
//! module) and resets the per-event state (`consume_event` and `phase`) between
//! events, while the `redraw` latch persists across the whole frame.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::Winsize;
use crate::cell::{self, Cell, CursorShape};
use crate::gwidth;
use crate::key::Key;
use crate::mouse::{self, Mouse};
use crate::unicode::{self, GraphemeIterator};
use crate::window::{ChildOptions, Window};

mod app;
mod border;
mod button;
mod center;
mod flex_column;
mod flex_row;
mod list_view;
mod padding;
mod rich_text;
mod scroll_bars;
mod scroll_view;
mod sized_box;
mod spinner;
mod split_view;
mod text;
mod text_field;

pub use crate::vxfw::app::{App, Options};
pub use crate::vxfw::border::{Border, BorderAlignment, BorderLabel};
pub use crate::vxfw::button::{Button, ButtonStyle};
pub use crate::vxfw::center::Center;
pub use crate::vxfw::flex_column::FlexColumn;
pub use crate::vxfw::flex_row::FlexRow;
pub use crate::vxfw::list_view::{ListSource, ListView, Source};
pub use crate::vxfw::padding::{PadValues, Padding};
pub use crate::vxfw::rich_text::{RichText, TextSpan};
pub use crate::vxfw::scroll_bars::ScrollBars;
pub use crate::vxfw::scroll_view::ScrollView;
pub use crate::vxfw::sized_box::SizedBox;
pub use crate::vxfw::spinner::Spinner;
pub use crate::vxfw::split_view::{Constrain, SplitView};
pub use crate::vxfw::text::{Overflow, Text, TextAlign, WidthBasis};
pub use crate::vxfw::text_field::TextField;

/// A reference-counted, interior-mutable handle to a widget.
///
/// The framework owns every widget through one of these. Identity is the `Rc`
/// pointer (see [`widget_eq`]); cloning a `WidgetRef` clones the `Rc`, not the
/// widget.
pub type WidgetRef = Rc<RefCell<dyn Widget>>;

/// Returns true if `a` and `b` refer to the same widget instance.
///
/// This is the framework's widget identity. It compares `Rc` pointers, so two
/// clones of the same `WidgetRef` are equal and distinct widgets never are.
pub fn widget_eq(a: &WidgetRef, b: &WidgetRef) -> bool {
    Rc::ptr_eq(a, b)
}

/// The widget interface.
///
/// `draw` takes `&mut self` so stateful widgets can update during layout, and
/// is infallible (allocation aborts on OOM in Rust). The event handlers default
/// to no-ops; a widget that participates in event dispatch overrides
/// [`wants_events`](Widget::wants_events) to return true, which is what
/// hit-testing and focus use to decide whether to include the widget.
pub trait Widget {
    /// Lays the widget out under `ctx`'s constraints and returns its surface.
    ///
    /// Build child surfaces with [`draw_widget`] so their identity is stamped.
    /// Do not stamp `Surface::widget` here: the caller's `draw_widget` does it.
    fn draw(&mut self, ctx: &DrawContext) -> Surface;

    /// Handles an event during the at-target or bubbling phase. Default no-op.
    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let _ = (ctx, event);
    }

    /// Handles an event during the capturing phase. Default no-op.
    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let _ = (ctx, event);
    }

    /// Whether this widget takes part in event dispatch. Hit-testing and focus
    /// only consider widgets that return true. Default false.
    fn wants_events(&self) -> bool {
        false
    }
}

/// Draws `w` and stamps the resulting surface with `w`'s identity.
///
/// This is the only correct way to produce a surface for a widget: it solves
/// the "a widget cannot name its own `Rc` inside `draw`" problem by stamping
/// `Surface::widget` here, after `draw` returns. Composite widgets call it for
/// each child and the App calls it for the root.
pub fn draw_widget(w: &WidgetRef, ctx: &DrawContext) -> Surface {
    let mut surface = w.borrow_mut().draw(ctx);
    surface.widget = Some(Rc::clone(w));
    surface
}

/// The user-facing event delivered to widgets.
///
/// NOTE: This is distinct from the internal [`crate::event::Event`] superset
/// the parser produces. The App converts the reader's events into this type and
/// synthesizes the rest ([`Event::Tick`], [`Event::Init`], the mouse
/// enter/leave pair).
#[derive(Debug, Clone)]
pub enum Event {
    KeyPress(Key),
    KeyRelease(Key),
    Mouse(Mouse),
    /// The window gained focus.
    FocusIn,
    /// The window lost focus.
    FocusOut,
    /// Bracketed-paste start.
    PasteStart,
    /// Bracketed-paste end.
    PasteEnd,
    /// OSC 52 paste payload.
    Paste(String),
    /// OSC 4/10/11/12 color response.
    ColorReport(cell::Report),
    /// Light/dark OS theme change.
    ColorScheme(cell::Scheme),
    /// The window size changed. Always delivered once when the App starts.
    Winsize(Winsize),
    /// A custom event posted by the application.
    App(UserEvent),
    /// Fired by a [`Command::Tick`] reaching its deadline.
    Tick,
    /// Sent once when the application starts.
    Init,
    /// The mouse left the widget.
    MouseLeave,
    /// The mouse entered the widget.
    MouseEnter,
}

/// A custom application event.
///
/// NOTE: `data` is an `Rc<dyn Any>` rather than upstream's opaque pointer. The
/// receiver downcasts it back to the concrete payload. Because `Rc` is not
/// `Send`, an `Event::App` cannot cross the reader thread (it is posted and
/// consumed on the App thread), unlike the reader-produced events.
#[derive(Clone)]
pub struct UserEvent {
    pub name: String,
    pub data: Option<Rc<dyn std::any::Any>>,
}

impl std::fmt::Debug for UserEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The payload is opaque, so we print only the name and whether a
        // payload is attached.
        f.debug_struct("UserEvent")
            .field("name", &self.name)
            .field("has_data", &self.data.is_some())
            .finish()
    }
}

/// A scheduled callback: deliver [`Event::Tick`] to `widget` once `deadline`
/// passes.
#[derive(Clone)]
pub struct Tick {
    pub deadline: Instant,
    pub widget: WidgetRef,
}

impl Tick {
    /// Builds a [`Command::Tick`] firing `ms` milliseconds from now.
    pub fn in_ms(ms: u32, widget: WidgetRef) -> Command {
        Command::Tick(Tick {
            deadline: Instant::now() + Duration::from_millis(u64::from(ms)),
            widget,
        })
    }

    /// Orders ticks with the latest deadline first and the soonest last.
    ///
    /// NOTE: This inverts the natural deadline ordering on purpose. The App
    /// keeps `timers` sorted with this comparator and pops from the end, so the
    /// soonest-due tick is the one it pops first. Mirrors upstream's `lessThan`.
    pub fn cmp_by_deadline_desc(a: &Tick, b: &Tick) -> Ordering {
        b.deadline.cmp(&a.deadline)
    }
}

/// A side effect a widget requests from the App by pushing it onto
/// [`EventContext::cmds`]. The App drains and applies these between dispatch
/// steps.
pub enum Command {
    /// Schedule a tick callback.
    Tick(Tick),
    /// Change the mouse shape. Implies a redraw.
    SetMouseShape(mouse::Shape),
    /// Request that this widget receive focus.
    RequestFocus(WidgetRef),
    /// Copy text to the host clipboard via OSC 52. Silently fails if the
    /// terminal does not support it.
    CopyToClipboard(String),
    /// Set the terminal title.
    SetTitle(String),
    /// Queue a full-screen refresh. Implies a redraw.
    QueueRefresh,
    /// Send a system notification.
    Notify { title: Option<String>, body: String },
    /// Query a terminal color.
    QueryColor(cell::Kind),
}

/// The phase of event propagation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Root-to-target, inclusive of the target.
    Capturing,
    /// The target only.
    AtTarget,
    /// Target-exclusive, back up to the root.
    Bubbling,
}

/// The bus a widget uses while handling an event: the propagation phase, an
/// outgoing command queue, and the latches the App reads after dispatch.
///
/// Latching contract: the App resets `consume_event` and `phase` between events
/// (the per-event state), but `redraw` is a per-frame latch it clears only once
/// it has drawn. So a handler that consumes one event does not leak that
/// consumption to the next event, while a redraw request from any handler
/// survives until the frame is drawn.
pub struct EventContext {
    pub phase: Phase,
    pub cmds: Vec<Command>,
    /// The event was handled; stop propagating it.
    pub consume_event: bool,
    /// Redraw the UI this frame.
    pub redraw: bool,
    /// Quit the application.
    pub quit: bool,
}

impl EventContext {
    /// A fresh context in the capturing phase with no pending redraw, matching
    /// the App's per-frame starting state.
    pub fn new() -> Self {
        Self {
            phase: Phase::Capturing,
            cmds: Vec::new(),
            consume_event: false,
            redraw: false,
            quit: false,
        }
    }

    /// Queues `cmd` for the App to apply.
    pub fn add_cmd(&mut self, cmd: Command) {
        self.cmds.push(cmd);
    }

    /// Schedules a tick for `widget` `ms` milliseconds from now.
    pub fn tick(&mut self, ms: u32, widget: WidgetRef) {
        self.add_cmd(Tick::in_ms(ms, widget));
    }

    /// Marks the event consumed and requests a redraw.
    pub fn consume_and_redraw(&mut self) {
        self.consume_event = true;
        self.redraw = true;
    }

    /// Marks the event consumed without requesting a redraw.
    pub fn consume_event(&mut self) {
        self.consume_event = true;
    }

    /// Requests a mouse-shape change (implies a redraw).
    pub fn set_mouse_shape(&mut self, shape: mouse::Shape) {
        self.add_cmd(Command::SetMouseShape(shape));
        self.redraw = true;
    }

    /// Requests focus for `widget`.
    pub fn request_focus(&mut self, widget: WidgetRef) {
        self.add_cmd(Command::RequestFocus(widget));
    }

    /// Copies `content` to the host clipboard.
    pub fn copy_to_clipboard(&mut self, content: String) {
        self.add_cmd(Command::CopyToClipboard(content));
    }

    /// Sets the terminal title.
    pub fn set_title(&mut self, title: String) {
        self.add_cmd(Command::SetTitle(title));
    }

    /// Queues a full-screen refresh (implies a redraw).
    pub fn queue_refresh(&mut self) {
        self.add_cmd(Command::QueueRefresh);
        self.redraw = true;
    }

    /// Sends a system notification.
    pub fn send_notification(&mut self, title: Option<String>, body: String) {
        self.add_cmd(Command::Notify { title, body });
    }

    /// Queries a terminal color.
    pub fn query_color(&mut self, kind: cell::Kind) {
        self.add_cmd(Command::QueryColor(kind));
    }
}

impl Default for EventContext {
    fn default() -> Self {
        Self::new()
    }
}

/// The layout constraints and measurement helpers passed down the widget tree
/// during a draw.
///
/// NOTE(D8): `width_method` is a field, not a file-scoped global. Upstream keeps
/// it in a mutable global; we thread it through the context so concurrent or
/// nested draws cannot race on it.
#[derive(Debug, Clone, Copy)]
pub struct DrawContext {
    /// Minimum size the widget must fill.
    pub min: Size,
    /// Maximum size the widget may occupy. A `None` dimension is unbounded.
    pub max: MaxSize,
    /// Size of one cell in pixels.
    pub cell_size: Size,
    /// The width-measurement method for [`string_width`](DrawContext::string_width).
    pub width_method: gwidth::Method,
}

impl DrawContext {
    /// Measures the display width of `str` in cells.
    pub fn string_width(&self, s: &str) -> usize {
        usize::from(gwidth::gwidth(s, self.width_method))
    }

    /// Iterates the grapheme clusters of `s`.
    pub fn grapheme_iterator<'a>(&self, s: &'a str) -> GraphemeIterator<'a> {
        unicode::grapheme_iterator(s)
    }

    /// Returns a copy of this context with new constraints, keeping the cell
    /// size and width method.
    pub fn with_constraints(&self, min: Size, max: MaxSize) -> DrawContext {
        DrawContext {
            min,
            max,
            cell_size: self.cell_size,
            width_method: self.width_method,
        }
    }
}

/// A fixed size in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    pub width: u16,
    pub height: u16,
}

/// A maximum size in cells, where a `None` dimension is unbounded (infinite).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MaxSize {
    pub width: Option<u16>,
    pub height: Option<u16>,
}

impl MaxSize {
    /// Whether `row` falls outside this height. A `None` height is infinite and
    /// always returns false.
    pub fn outside_height(&self, row: u16) -> bool {
        match self.height {
            Some(max) => row >= max,
            None => false,
        }
    }

    /// Whether `col` falls outside this width. A `None` width is infinite and
    /// always returns false.
    pub fn outside_width(&self, col: u16) -> bool {
        match self.width {
            Some(max) => col >= max,
            None => false,
        }
    }

    /// The size as a fixed [`Size`]. Asserts both dimensions are bounded.
    pub fn size(&self) -> Size {
        Size {
            width: self.width.expect("MaxSize::size requires a bounded width"),
            height: self
                .height
                .expect("MaxSize::size requires a bounded height"),
        }
    }

    /// A bounded [`MaxSize`] equal to `other`.
    pub fn from_size(other: Size) -> MaxSize {
        MaxSize {
            width: Some(other.width),
            height: Some(other.height),
        }
    }
}

/// A child of a [`FlexColumn`]/[`FlexRow`] with its flex factor.
///
/// A `flex` of zero gives the child its inherent size; any positive value
/// proportions the remaining space among the flexible children.
#[derive(Clone)]
pub struct FlexItem {
    pub widget: WidgetRef,
    pub flex: u8,
}

impl FlexItem {
    /// Builds a flex item for `child` with factor `flex`.
    pub fn init(child: WidgetRef, flex: u8) -> FlexItem {
        FlexItem {
            widget: child,
            flex,
        }
    }
}

/// A point in surface-local cell coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub row: u16,
    pub col: u16,
}

/// A point relative to a parent surface. Signed because a child can sit above
/// or left of its parent's origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelativePoint {
    pub row: i32,
    pub col: i32,
}

/// A widget hit by a point test, with the point translated into that widget's
/// local coordinates.
#[derive(Clone)]
pub struct HitResult {
    pub local: Point,
    pub widget: WidgetRef,
}

/// A widget's requested cursor position and shape, in surface-local
/// coordinates.
#[derive(Debug, Clone, Copy)]
pub struct CursorState {
    pub row: u16,
    pub col: u16,
    pub shape: CursorShape,
}

/// The default cell a fresh surface buffer is filled with: an explicit
/// "default" cell, matching upstream's `.{ .default = true }`.
fn default_cell() -> Cell {
    Cell {
        default: true,
        ..Cell::default()
    }
}

/// A laid-out widget: its size, the cells it drew, its children, and the widget
/// identity stamped by [`draw_widget`].
///
/// We own the cell buffer and the child list with plain `Vec`s (D3: no
/// per-frame arena for the first cut). The App's `MouseHandler` keeps the last
/// frame's `Surface` so it survives into the next frame for hit-testing; owning
/// the tree is what makes that survival trivial. A bump arena can replace these
/// `Vec`s later if 60 fps allocation churn shows up in a profile.
#[derive(Clone)]
pub struct Surface {
    /// The size of this surface.
    pub size: Size,
    /// The widget this surface belongs to, stamped by [`draw_widget`]. `None`
    /// until stamped.
    pub widget: Option<WidgetRef>,
    /// The cursor this surface requests, if any.
    pub cursor: Option<CursorState>,
    /// The cells, `size.width * size.height` long, or empty for a surface that
    /// only positions children.
    pub buffer: Vec<Cell>,
    /// Child surfaces, positioned relative to this surface's origin.
    pub children: Vec<SubSurface>,
}

impl Surface {
    /// An empty 0x0 surface with no widget, buffer, or children.
    pub fn empty() -> Surface {
        Surface {
            size: Size::default(),
            widget: None,
            cursor: None,
            buffer: Vec::new(),
            children: Vec::new(),
        }
    }

    /// A surface of `size` with a buffer of default cells and no children.
    pub fn with_size(size: Size) -> Surface {
        let len = usize::from(size.width) * usize::from(size.height);
        Surface {
            size,
            widget: None,
            cursor: None,
            buffer: vec![default_cell(); len],
            children: Vec::new(),
        }
    }

    /// A surface of `size` with a buffer of default cells and the given
    /// children.
    pub fn with_children(size: Size, children: Vec<SubSurface>) -> Surface {
        let len = usize::from(size.width) * usize::from(size.height);
        Surface {
            size,
            widget: None,
            cursor: None,
            buffer: vec![default_cell(); len],
            children,
        }
    }

    /// Writes `cell` at the local `(col, row)`. Out-of-bounds writes are
    /// silently clipped.
    pub fn write_cell(&mut self, col: u16, row: u16, cell: Cell) {
        if self.size.width <= col || self.size.height <= row {
            return;
        }
        let i = usize::from(row) * usize::from(self.size.width) + usize::from(col);
        debug_assert!(i < self.buffer.len());
        self.buffer[i] = cell;
    }

    /// Reads the cell at the local `(col, row)`. Asserts the position is in
    /// bounds.
    pub fn read_cell(&self, col: u16, row: u16) -> Cell {
        assert!(col < self.size.width && row < self.size.height);
        let i = usize::from(row) * usize::from(self.size.width) + usize::from(col);
        assert!(i < self.buffer.len());
        self.buffer[i].clone()
    }

    /// Returns a copy of this surface with the same width and its buffer trimmed
    /// to `height` rows. Asserts `height <= self.size.height`.
    pub fn trim_height(&self, height: u16) -> Surface {
        assert!(height <= self.size.height);
        let len = usize::from(self.size.width) * usize::from(height);
        Surface {
            size: Size {
                width: self.size.width,
                height,
            },
            widget: self.widget.clone(),
            cursor: self.cursor,
            buffer: self.buffer[..len].to_vec(),
            children: self.children.clone(),
        }
    }

    /// Appends every widget intersecting `point` to `list`, deepest last.
    ///
    /// `point` is in this surface's local coordinates and is translated into
    /// each child's local frame on the way down. Only widgets that
    /// [`want events`](Widget::wants_events) are added. Asserts `point` lies
    /// within this surface.
    pub fn hit_test(&self, point: Point, list: &mut Vec<HitResult>) {
        debug_assert!(point.col < self.size.width && point.row < self.size.height);
        if let Some(w) = &self.widget {
            if w.borrow().wants_events() {
                list.push(HitResult {
                    local: point,
                    widget: Rc::clone(w),
                });
            }
        }
        for child in &self.children {
            if !child.contains_point(point) {
                continue;
            }
            // `contains_point` guarantees the difference is in `[0, size)`, so
            // both subtractions are non-negative and fit `u16`.
            let child_point = Point {
                row: u16::try_from(i32::from(point.row) - child.origin.row)
                    .expect("hit point within child rows"),
                col: u16::try_from(i32::from(point.col) - child.origin.col)
                    .expect("hit point within child cols"),
            };
            child.surface.hit_test(child_point, list);
        }
    }

    /// Blits this surface onto `win`, then renders its children top-of-stack
    /// last.
    ///
    /// The cursor is shown only when this surface owns the `focused` widget.
    /// Children are rendered in ascending z-index, so higher z-index children
    /// paint over lower ones.
    pub fn render(&self, win: Window<'_>, focused: Option<&WidgetRef>) {
        if !self.buffer.is_empty() {
            debug_assert_eq!(
                self.buffer.len(),
                usize::from(self.size.width) * usize::from(self.size.height)
            );
            let width = usize::from(self.size.width);
            for (i, cell) in self.buffer.iter().enumerate() {
                let row = i / width;
                let col = i % width;
                win.write_cell(
                    u16::try_from(col).expect("col fits u16"),
                    u16::try_from(row).expect("row fits u16"),
                    cell.clone(),
                );
            }
        }

        if let (Some(cursor), Some(widget)) = (self.cursor, &self.widget) {
            if focused.is_some_and(|f| widget_eq(widget, f)) {
                win.show_cursor(cursor.col, cursor.row);
                win.set_cursor_shape(cursor.shape);
            }
        }

        // Render children in ascending z-index. We sort references rather than
        // the owned children so `render` can take `&self`; a stable sort keeps
        // insertion order among equal z-indexes.
        let mut order: Vec<&SubSurface> = self.children.iter().collect();
        order.sort_by_key(|child| child.z_index);
        for child in order {
            let child_win = win.child(ChildOptions {
                x_off: child.origin.col,
                y_off: child.origin.row,
                width: Some(child.surface.size.width),
                height: Some(child.surface.size.height),
                ..ChildOptions::default()
            });
            child.surface.render(child_win, focused);
        }
    }

    /// Whether this surface's size sits strictly between `min` and `max` on both
    /// axes. The strict inequalities mirror upstream exactly.
    pub fn satisfies_constraints(&self, min: Size, max: Size) -> bool {
        self.size.width < max.width
            && self.size.width > min.width
            && self.size.height < max.height
            && self.size.height > min.height
    }
}

/// A child surface positioned within its parent, with a stacking order.
#[derive(Clone)]
pub struct SubSurface {
    /// Origin relative to the parent surface.
    pub origin: RelativePoint,
    /// The child surface.
    pub surface: Surface,
    /// Stacking order among siblings; higher paints later.
    pub z_index: u8,
}

impl SubSurface {
    /// Whether `point` (in parent-local coordinates) falls within this child.
    ///
    /// The origin is inclusive and the far edge (origin + size) is exclusive.
    /// Coordinates are promoted to signed so a negative origin compares
    /// correctly.
    pub fn contains_point(&self, point: Point) -> bool {
        let col = i32::from(point.col);
        let row = i32::from(point.row);
        col >= self.origin.col
            && row >= self.origin.row
            && col < self.origin.col + i32::from(self.surface.size.width)
            && row < self.origin.row + i32::from(self.surface.size.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_surface_contains_point() {
        let surf = SubSurface {
            origin: RelativePoint { row: 2, col: 2 },
            surface: Surface {
                size: Size {
                    width: 10,
                    height: 10,
                },
                widget: None,
                cursor: None,
                buffer: Vec::new(),
                children: Vec::new(),
            },
            z_index: 0,
        };

        assert!(surf.contains_point(Point { row: 2, col: 2 }));
        assert!(surf.contains_point(Point { row: 3, col: 3 }));
        assert!(surf.contains_point(Point { row: 11, col: 11 }));

        assert!(!surf.contains_point(Point { row: 1, col: 1 }));
        assert!(!surf.contains_point(Point { row: 12, col: 12 }));
        assert!(!surf.contains_point(Point { row: 2, col: 12 }));
        assert!(!surf.contains_point(Point { row: 12, col: 2 }));
    }

    #[test]
    fn surface_satisfies_constraints() {
        let surf = Surface {
            size: Size {
                width: 10,
                height: 10,
            },
            widget: None,
            cursor: None,
            buffer: Vec::new(),
            children: Vec::new(),
        };

        assert!(surf.satisfies_constraints(
            Size {
                width: 1,
                height: 1
            },
            Size {
                width: 20,
                height: 20
            }
        ));
        assert!(!surf.satisfies_constraints(
            Size {
                width: 10,
                height: 10
            },
            Size {
                width: 20,
                height: 20
            }
        ));
        assert!(!surf.satisfies_constraints(
            Size {
                width: 1,
                height: 1
            },
            Size {
                width: 10,
                height: 10
            }
        ));
    }

    /// Convention enforcer (the reframed upstream "all widgets have a doctest"
    /// meta-test). Walks `src/vxfw/` and asserts every widget module file
    /// carries a `#[test]` whose name matches the module file stem (its
    /// "doctest"). The framework core (`vxfw.rs`) and the `App` runtime
    /// (`app.rs`) are excluded. This is a lightweight string scan, not an AST
    /// parse: it only proves the test exists, which is enough to fail CI early
    /// when a widget lands without one. It passes vacuously until widgets land.
    #[test]
    fn all_widgets_have_a_doctest() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/vxfw");
        let excludes = ["app"];
        let entries = std::fs::read_dir(&dir).expect("read src/vxfw");
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("module file stem");
            if excludes.contains(&stem) {
                continue;
            }
            let data = std::fs::read_to_string(&path).expect("read widget module");
            let needle = format!("fn {stem}(");
            assert!(
                data.contains(&needle),
                "widget module `{stem}` has no doctest named `{stem}`"
            );
        }
    }
}
