#![cfg(unix)]

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;

use vaxis::cell::{Cell, Character};
use vaxis::mouse::{Button, Mouse, Shape, Type};
use vaxis::tty::TestTty;
use vaxis::vaxis::{Options as VaxisOptions, Vaxis};
use vaxis::vxfw::{
    AsyncApp, Constrain, DrawContext, Event, EventContext, MaxSize, Options, RelativePoint, Size,
    SplitView, SubSurface, Surface, Widget, WidgetRef, draw_widget, to_widget_ref,
};

const COL: u16 = 4;
const ROW: u16 = 2;
const WIDTH: u16 = 24;
const HEIGHT: u16 = 3;

struct Pane {
    glyph: &'static str,
    received: Vec<Mouse>,
}

impl Widget for Pane {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let size = ctx.max.size();
        let mut surface = Surface::with_size(size);
        for row in 0..size.height {
            for col in 0..size.width {
                surface.write_cell(
                    col,
                    row,
                    Cell {
                        char: Character::new(self.glyph, 1),
                        ..Cell::default()
                    },
                );
            }
        }
        surface
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            Event::MouseEnter => ctx.set_mouse_shape(Shape::Pointer),
            Event::MouseLeave => ctx.set_mouse_shape(Shape::Default),
            _ => {}
        }
        if let Event::Mouse(mouse) = event {
            self.received.push(*mouse);
            // A split that relies on bubbling from its panes cannot resize here.
            ctx.consume_event = true;
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

struct Frame {
    split: WidgetRef,
    width: u16,
}

impl Widget for Frame {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let size = Size {
            width: self.width,
            height: HEIGHT,
        };
        Surface::with_children(
            ctx.max.size(),
            vec![SubSurface {
                origin: RelativePoint {
                    row: i32::from(ROW),
                    col: i32::from(COL),
                },
                surface: draw_widget(
                    &self.split,
                    &ctx.with_constraints(size, MaxSize::from_size(size)),
                ),
                z_index: 0,
            }],
        )
    }
}

struct Harness {
    app: AsyncApp,
    root: WidgetRef,
    frame: Rc<RefCell<Frame>>,
    split: Rc<RefCell<SplitView>>,
    panes: [Rc<RefCell<Pane>>; 2],
    _write_fd: OwnedFd,
}

impl Harness {
    async fn new(constrain: Constrain) -> Self {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("pipe");
        // Answer capability detection without waiting for the DA1 timeout.
        let reply = b"\x1b[?c";
        assert_eq!(nix::unistd::write(&write_fd, reply).unwrap(), reply.len());
        let panes = ["L", "R"].map(|glyph| {
            Rc::new(RefCell::new(Pane {
                glyph,
                received: Vec::new(),
            }))
        });
        let mut split = SplitView::new(
            to_widget_ref(Rc::clone(&panes[0])),
            to_widget_ref(Rc::clone(&panes[1])),
            8,
        );
        split.constrain = constrain;
        split.min_width = 3;
        split.max_width = Some(14);
        split.min_other_width = 6;
        let split = Rc::new(RefCell::new(split));
        let frame = Rc::new(RefCell::new(Frame {
            split: to_widget_ref(Rc::clone(&split)),
            width: WIDTH,
        }));
        let root = to_widget_ref(Rc::clone(&frame));
        let mut app = AsyncApp::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
            read_fd,
        );
        app.init(Rc::clone(&root), Options::default())
            .await
            .expect("init");
        Self {
            app,
            root,
            frame,
            split,
            panes,
            _write_fd: write_fd,
        }
    }

    fn mouse(&mut self, kind: Type, row: i16, col: i16) {
        self.button(
            kind,
            if kind == Type::Motion {
                Button::None
            } else {
                Button::Left
            },
            row,
            col,
        );
    }

    fn button(&mut self, kind: Type, button: Button, row: i16, col: i16) {
        self.app.handle_input(Event::Mouse(Mouse {
            row,
            col,
            kind,
            button,
            mods: Default::default(),
            xoffset: 0,
            yoffset: 0,
        }));
    }

    fn paint(&mut self) {
        self.app.render_if_needed(&self.root).expect("render");
    }

    fn redraw(&mut self) {
        self.app.request_redraw();
        self.paint();
    }

    fn assert_layout(&mut self, width: u16, divider: char) {
        self.paint();
        let lhs = match self.split.borrow().constrain {
            Constrain::Lhs => width,
            Constrain::Rhs => WIDTH - width - 1,
        };
        let expected = format!(
            "{}{}{}",
            "L".repeat(usize::from(lhs)),
            divider,
            "R".repeat(usize::from(WIDTH - lhs - 1)),
        );
        let screen = self.app.vaxis().screen.borrow();
        for row in ROW..ROW + HEIGHT {
            let actual: String = (COL..COL + WIDTH)
                .map(|col| {
                    screen
                        .read_cell(col, row)
                        .unwrap()
                        .char
                        .grapheme()
                        .to_owned()
                })
                .collect();
            assert_eq!(actual, expected, "row {row}");
        }
    }

    fn assert_cursor(&mut self, expected: Shape) {
        assert_eq!(self.app.vaxis().screen.borrow().mouse_shape, expected);
    }

    fn assert_panes_untouched(&self) {
        for pane in &self.panes {
            assert!(pane.borrow().received.is_empty());
        }
    }

    fn divider_col(&self, width: u16) -> i16 {
        let local = match self.split.borrow().constrain {
            Constrain::Lhs => width,
            Constrain::Rhs => WIDTH - width - 1,
        };
        i16::try_from(COL + local).unwrap()
    }
}

#[tokio::test]
async fn drag_crosses_consuming_panes_and_clamps_both_sides_in_signed_coordinates() {
    for constrain in [Constrain::Lhs, Constrain::Rhs] {
        let mut h = Harness::new(constrain).await;
        h.assert_layout(8, '│');
        h.mouse(Type::Press, 3, h.divider_col(8));
        for width in [12, 5] {
            h.mouse(Type::Drag, 3, h.divider_col(width));
            h.assert_layout(width, '┃');
        }
        for (col, lhs_width, rhs_width) in [(-10, 3, 14), (100, 14, 3)] {
            h.mouse(Type::Drag, -8, col);
            h.assert_layout(
                if constrain == Constrain::Lhs {
                    lhs_width
                } else {
                    rhs_width
                },
                '┃',
            );
            h.assert_cursor(Shape::EwResize);
        }
        // The release is a new position, not a repeat of the last drag report.
        h.mouse(Type::Release, 50, h.divider_col(10));
        h.assert_layout(10, '│');
        h.assert_cursor(Shape::Default);
        h.assert_panes_untouched();

        h.split.borrow_mut().max_width = None;
        h.redraw();
        h.mouse(Type::Press, 3, h.divider_col(10));
        let beyond = if constrain == Constrain::Lhs {
            100
        } else {
            -10
        };
        h.mouse(Type::Drag, -8, beyond);
        h.assert_layout(17, '┃'); // 24 columns minus divider and other pane's 6.
        h.mouse(Type::Release, -8, beyond);
        h.assert_layout(17, '│');
        h.assert_panes_untouched();
        h.app.shutdown().await;
    }
}

#[tokio::test]
async fn hover_leave_release_and_focus_loss_restore_divider_and_cursor() {
    let mut h = Harness::new(Constrain::Lhs).await;
    h.assert_layout(8, '│');
    h.mouse(Type::Motion, 3, h.divider_col(8));
    h.assert_layout(8, '┃');
    h.assert_cursor(Shape::EwResize);
    h.mouse(Type::Motion, -1, -1);
    h.assert_layout(8, '│');
    h.assert_cursor(Shape::Default);

    h.mouse(Type::Press, 3, h.divider_col(8));
    h.mouse(Type::Release, 3, h.divider_col(12));
    h.assert_layout(12, '┃');
    h.assert_cursor(Shape::EwResize);

    h.mouse(Type::Press, 3, h.divider_col(12));
    h.mouse(Type::Drag, -1, h.divider_col(10));
    h.assert_layout(10, '┃');
    h.assert_cursor(Shape::EwResize);
    h.app.handle_input(Event::FocusOut);
    h.assert_layout(10, '│');
    h.assert_cursor(Shape::Default);
    h.app.handle_input(Event::FocusIn);
    h.mouse(Type::Release, -1, h.divider_col(5));
    h.assert_layout(10, '│');

    // A fresh gesture works after cancellation, and release outside clears hover.
    h.mouse(Type::Press, 3, h.divider_col(10));
    h.mouse(Type::Release, -1, h.divider_col(7));
    h.assert_layout(7, '│');
    h.assert_cursor(Shape::Default);
    h.assert_panes_untouched();
    h.app.shutdown().await;
}

#[tokio::test]
async fn only_left_divider_press_captures_and_ordinary_pane_clicks_still_arrive() {
    let mut h = Harness::new(Constrain::Lhs).await;
    for button in [Button::Right, Button::Middle] {
        h.button(Type::Press, button, 3, h.divider_col(8));
        h.button(Type::Drag, button, 3, 6);
        h.button(Type::Release, button, 3, 6);
        h.assert_layout(8, '│');
        assert_eq!(h.panes[0].borrow().received.len(), 2);
        h.panes[0].borrow_mut().received.clear();
    }
    h.mouse(Type::Press, 3, h.divider_col(8));
    h.mouse(Type::Release, -1, h.divider_col(8));
    h.assert_panes_untouched();
    for (index, col) in [(0, 6), (1, 24)] {
        h.mouse(Type::Press, 3, col);
        h.mouse(Type::Release, 3, col);
        let pane = h.panes[index].borrow();
        assert_eq!(pane.received.len(), 2);
        assert_eq!(pane.received[0].kind, Type::Press);
        assert_eq!(pane.received[0].button, Button::Left);
        assert_eq!(pane.received[1].kind, Type::Release);
    }
    h.assert_layout(8, '│');
    h.app.shutdown().await;
}

#[tokio::test]
async fn queued_drag_reports_use_the_last_drawn_geometry() {
    for constrain in [Constrain::Lhs, Constrain::Rhs] {
        let mut h = Harness::new(constrain).await;
        h.assert_layout(8, '│');
        h.mouse(Type::Press, 3, h.divider_col(8));
        // No repaint between reports. Each position is relative to the same frame.
        for width in [10, 12, 9] {
            h.mouse(Type::Drag, 3, h.divider_col(width));
        }
        h.assert_layout(9, '┃');
        // The next batch is relative to the newly drawn divider instead.
        h.mouse(Type::Drag, 3, h.divider_col(7));
        h.mouse(Type::Drag, 3, h.divider_col(11));
        h.mouse(Type::Release, -1, h.divider_col(10));
        h.assert_layout(10, '│');
        h.assert_panes_untouched();
        h.app.shutdown().await;
    }
}

#[tokio::test]
async fn configured_width_and_impossible_minima_stay_inside_the_viewport() {
    for constrain in [Constrain::Lhs, Constrain::Rhs] {
        let mut h = Harness::new(constrain).await;
        h.split.borrow_mut().set_width(u16::MAX);
        h.redraw();
        h.assert_layout(14, '│');
        for width in [0, 1, 2, 4] {
            h.frame.borrow_mut().width = width;
            h.split.borrow_mut().set_width(u16::MAX);
            h.redraw();
            let screen = h.app.vaxis().screen.borrow();
            for row in ROW..ROW + HEIGHT {
                let mut dividers = 0;
                for col in 0..screen.width {
                    let cell = screen.read_cell(col, row).unwrap();
                    let glyph = cell.char.grapheme();
                    if (COL..COL + width).contains(&col) {
                        assert!(matches!(glyph, "L" | "R" | "│"));
                        dividers += usize::from(glyph == "│");
                    } else {
                        assert_eq!(glyph, " ", "content escaped width {width} at {col}");
                    }
                }
                assert_eq!(dividers, usize::from(width > 0));
            }
        }
        h.app.shutdown().await;
    }
}
