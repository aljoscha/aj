//! Shared test helpers for inspecting drawn surfaces.

use vaxis::cell::Cell;
use vaxis::vxfw::{DrawContext, MaxSize, Size, Surface};

use crate::subagent_box::surface_rows;

/// A draw context bounded to `width`, optionally bounded in height.
pub(crate) fn draw_ctx(width: u16, height: Option<u16>) -> DrawContext {
    DrawContext {
        min: Size {
            width: 0,
            height: 0,
        },
        max: MaxSize {
            width: Some(width),
            height,
        },
        cell_size: Size {
            width: 10,
            height: 20,
        },
        width_method: vaxis::gwidth::Method::Unicode,
    }
}

/// Composite a surface tree into a flat cell grid, the way
/// `Surface::render` paints it.
pub(crate) fn flatten(surface: &Surface) -> Vec<Vec<Cell>> {
    surface_rows(surface)
}

/// The visible text of each composited row, right-trimmed.
pub(crate) fn rows(surface: &Surface) -> Vec<String> {
    flatten(surface)
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| c.char.grapheme())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// Mount a widget in the real input dispatcher at a fixed test viewport.
/// Keep the returned write fd alive so the input source does not reach EOF.
pub(crate) async fn widget_app(
    child: vaxis::vxfw::WidgetRef,
    focus: vaxis::vxfw::WidgetRef,
    size: Size,
) -> (
    vaxis::vxfw::AsyncApp,
    vaxis::vxfw::WidgetRef,
    std::os::fd::OwnedFd,
) {
    use std::{cell::RefCell, rc::Rc};
    use vaxis::vxfw::{Event, EventContext, Widget, WidgetRef, draw_widget};
    struct Root {
        child: WidgetRef,
        focus: WidgetRef,
        size: Size,
    }
    impl Widget for Root {
        fn draw(&mut self, ctx: &DrawContext) -> Surface {
            Surface::with_children(
                self.size,
                vec![vaxis::vxfw::SubSurface {
                    origin: vaxis::vxfw::RelativePoint { row: 0, col: 0 },
                    surface: draw_widget(
                        &self.child,
                        &ctx.with_constraints(
                            Size::default(),
                            MaxSize {
                                width: Some(self.size.width),
                                height: Some(self.size.height),
                            },
                        ),
                    ),
                    z_index: 0,
                }],
            )
        }
        fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
            if matches!(event, Event::Init) {
                ctx.request_focus(Rc::clone(&self.focus));
            }
        }
        fn wants_events(&self) -> bool {
            true
        }
    }
    let root: WidgetRef = Rc::new(RefCell::new(Root { child, focus, size }));
    let (read, write) = nix::unistd::pipe().unwrap();
    nix::unistd::write(&write, b"\x1b[?c").unwrap();
    let mut app = vaxis::vxfw::AsyncApp::new(
        vaxis::vaxis::Vaxis::new(vaxis::vaxis::Options::default()),
        Box::new(vaxis::tty::TestTty::new()),
        read,
    );
    app.init(Rc::clone(&root), vaxis::vxfw::Options::default())
        .await
        .unwrap();
    (app, root, write)
}
