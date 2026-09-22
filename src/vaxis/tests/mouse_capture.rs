#![cfg(unix)]

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;

use vaxis::mouse::{Button, Mouse, Type};
use vaxis::tty::TestTty;
use vaxis::vaxis::{Options as VaxisOptions, Vaxis};
use vaxis::vxfw::{
    AsyncApp, DrawContext, Event, EventContext, Options, RelativePoint, Size, SubSurface, Surface,
    Widget, WidgetRef, draw_widget, to_widget_ref,
};

#[derive(Debug, PartialEq, Eq)]
enum Received {
    Mouse(Mouse),
    Lost,
}

#[derive(Default)]
struct Pane {
    captures: bool,
    received: Vec<Received>,
    leaves: usize,
}

impl Widget for Pane {
    fn draw(&mut self, _ctx: &DrawContext) -> Surface {
        Surface::with_size(Size {
            width: 8,
            height: 5,
        })
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            Event::Mouse(mouse) => {
                self.received.push(Received::Mouse(*mouse));
                if self.captures && mouse.kind == Type::Press {
                    ctx.capture_mouse();
                }
            }
            Event::MouseCaptureLost => self.received.push(Received::Lost),
            Event::MouseLeave => self.leaves += 1,
            _ => {}
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

struct TwoPanes {
    owner: Rc<RefCell<Pane>>,
    sibling: Rc<RefCell<Pane>>,
    owner_origin: RelativePoint,
    show_owner: bool,
    capturing: Vec<Mouse>,
    bubbling: Vec<Mouse>,
}

impl Widget for TwoPanes {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let mut surface = Surface::with_size(Size {
            width: 30,
            height: 12,
        });
        if self.show_owner {
            surface.children.push(SubSurface {
                origin: self.owner_origin,
                surface: draw_widget(&to_widget_ref(Rc::clone(&self.owner)), ctx),
                z_index: 0,
            });
        }
        surface.children.push(SubSurface {
            origin: RelativePoint { row: 2, col: 16 },
            surface: draw_widget(&to_widget_ref(Rc::clone(&self.sibling)), ctx),
            z_index: 0,
        });
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        if let Event::Mouse(mouse) = event {
            self.capturing.push(*mouse);
        }
        if let Event::App(event) = event {
            let target = if event.name == "focus-owner" {
                &self.owner
            } else {
                &self.sibling
            };
            ctx.request_focus(to_widget_ref(Rc::clone(target)));
        }
    }

    fn handle_event(&mut self, _ctx: &mut EventContext, event: &Event) {
        if let Event::Mouse(mouse) = event {
            self.bubbling.push(*mouse);
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

struct Harness {
    app: AsyncApp,
    tree: Rc<RefCell<TwoPanes>>,
    root: WidgetRef,
    owner: Rc<RefCell<Pane>>,
    sibling: Rc<RefCell<Pane>>,
    _write_fd: OwnedFd,
}

impl Harness {
    async fn new() -> Self {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("pipe");
        // Answer capability detection without waiting for the DA1 timeout.
        let reply = b"\x1b[?c";
        assert_eq!(nix::unistd::write(&write_fd, reply).unwrap(), reply.len());
        let owner = Rc::new(RefCell::new(Pane {
            captures: true,
            ..Pane::default()
        }));
        let sibling = Rc::new(RefCell::new(Pane::default()));
        let tree = Rc::new(RefCell::new(TwoPanes {
            owner: Rc::clone(&owner),
            sibling: Rc::clone(&sibling),
            owner_origin: RelativePoint { row: 2, col: 4 },
            show_owner: true,
            capturing: Vec::new(),
            bubbling: Vec::new(),
        }));
        let root = to_widget_ref(Rc::clone(&tree));
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
            tree,
            root,
            owner,
            sibling,
            _write_fd: write_fd,
        }
    }

    fn mouse(&mut self, kind: Type, row: i16, col: i16) {
        self.app.handle_input(Event::Mouse(mouse(kind, row, col)));
    }

    fn redraw(&mut self) {
        self.app.request_redraw();
        self.app.render_if_needed(&self.root).expect("render");
    }
}

fn mouse(kind: Type, row: i16, col: i16) -> Mouse {
    Mouse {
        row,
        col,
        kind,
        button: if kind == Type::Motion {
            Button::None
        } else {
            Button::Left
        },
        mods: Default::default(),
        xoffset: 0,
        yoffset: 0,
    }
}

fn received(kind: Type, row: i16, col: i16) -> Received {
    Received::Mouse(mouse(kind, row, col))
}

#[tokio::test]
async fn capture_routes_outside_owner_until_release_then_restores_hit_testing() {
    let mut h = Harness::new().await;
    h.mouse(Type::Press, 3, 5);
    assert_eq!(h.owner.borrow().received, [received(Type::Press, 1, 1)]);
    assert!(
        h.tree.borrow().bubbling.is_empty(),
        "capture consumes press"
    );

    h.mouse(Type::Drag, 3, 17); // Over the sibling.
    assert_eq!(h.owner.borrow().leaves, 1, "leaving remains a hover event");
    h.mouse(Type::Drag, -1, -2);
    h.mouse(Type::Drag, 50, 90);
    h.mouse(Type::Release, 3, 17);
    assert_eq!(
        h.owner.borrow().received,
        [
            received(Type::Press, 1, 1),
            received(Type::Drag, 1, 13),
            received(Type::Drag, -3, -6),
            received(Type::Drag, 48, 86),
            received(Type::Release, 1, 13),
        ]
    );
    assert!(h.sibling.borrow().received.is_empty());
    let routed = [
        mouse(Type::Drag, 3, 17),
        mouse(Type::Drag, -1, -2),
        mouse(Type::Drag, 50, 90),
        mouse(Type::Release, 3, 17),
    ];
    assert_eq!(h.tree.borrow().capturing[1..], routed);
    assert_eq!(h.tree.borrow().bubbling, routed);

    h.mouse(Type::Press, 3, 17);
    h.mouse(Type::Release, 3, 17);
    assert_eq!(
        h.sibling.borrow().received,
        [received(Type::Press, 1, 1), received(Type::Release, 1, 1)]
    );
    assert_eq!(h.owner.borrow().received.len(), 5);
    h.app.shutdown().await;
}

#[tokio::test]
async fn wheel_input_keeps_hit_testing_without_replacing_capture() {
    let mut h = Harness::new().await;
    h.mouse(Type::Press, 3, 5);
    // Even a widget requesting capture for every press cannot own a wheel tick.
    h.sibling.borrow_mut().captures = true;
    for button in [
        Button::WheelUp,
        Button::WheelDown,
        Button::WheelLeft,
        Button::WheelRight,
    ] {
        h.app.handle_input(Event::Mouse(Mouse {
            button,
            ..mouse(Type::Press, 3, 17)
        }));
        assert_eq!(
            h.sibling.borrow().received.last(),
            Some(&Received::Mouse(Mouse {
                button,
                ..mouse(Type::Press, 1, 1)
            }))
        );
    }
    h.mouse(Type::Drag, 3, 17);
    h.mouse(Type::Release, 3, 17);
    assert_eq!(
        h.owner.borrow().received,
        [
            received(Type::Press, 1, 1),
            received(Type::Drag, 1, 13),
            received(Type::Release, 1, 13)
        ]
    );
    assert_eq!(
        h.sibling.borrow().received.len(),
        4,
        "only the wheel ticks reach the sibling"
    );
    h.app.shutdown().await;
}

#[tokio::test]
async fn removing_owner_from_rendered_tree_cancels_before_stale_release() {
    let mut h = Harness::new().await;
    h.mouse(Type::Press, 3, 5);
    h.tree.borrow_mut().show_owner = false;
    h.redraw();
    assert_eq!(
        h.owner.borrow().received,
        [received(Type::Press, 1, 1), Received::Lost]
    );

    h.mouse(Type::Release, 3, 5);
    h.mouse(Type::Press, 3, 17);
    h.mouse(Type::Release, 3, 17);
    assert_eq!(
        h.owner.borrow().received,
        [received(Type::Press, 1, 1), Received::Lost]
    );
    assert_eq!(
        h.sibling.borrow().received,
        [received(Type::Press, 1, 1), received(Type::Release, 1, 1)]
    );
    h.app.shutdown().await;
}

#[tokio::test]
async fn focus_loss_and_stale_gesture_signals_cancel_capture() {
    for cancellation in [
        Event::FocusOut,
        Event::Mouse(mouse(Type::Motion, 3, 17)),
        Event::Mouse(mouse(Type::Press, 3, 17)),
    ] {
        let mut h = Harness::new().await;
        h.mouse(Type::Press, 3, 5);
        h.app.handle_input(cancellation.clone());
        assert_eq!(
            h.owner.borrow().received,
            [received(Type::Press, 1, 1), Received::Lost],
            "{cancellation:?} must cancel capture"
        );
        h.app.handle_input(Event::FocusIn);
        h.mouse(Type::Release, 3, 17);
        assert_eq!(h.owner.borrow().received.len(), 2, "no stale release");
        let expected = match cancellation {
            Event::Mouse(m) => vec![received(m.kind, 1, 1), received(Type::Release, 1, 1)],
            _ => vec![],
        };
        assert_eq!(h.sibling.borrow().received, expected);
        h.app.shutdown().await;
    }
}

#[tokio::test]
async fn focus_requests_within_owner_preserve_capture_but_sibling_focus_cancels() {
    let mut h = Harness::new().await;
    h.mouse(Type::Press, 3, 5);
    h.app.post_app_event(vaxis::vxfw::UserEvent {
        name: "focus-owner".into(),
        data: None,
    });
    h.mouse(Type::Drag, 3, 17);
    assert_eq!(
        h.owner.borrow().received,
        [received(Type::Press, 1, 1), received(Type::Drag, 1, 13)]
    );
    h.app.post_app_event(vaxis::vxfw::UserEvent {
        name: "focus-sibling".into(),
        data: None,
    });
    h.mouse(Type::Release, 3, 17);
    assert_eq!(
        h.owner.borrow().received,
        [
            received(Type::Press, 1, 1),
            received(Type::Drag, 1, 13),
            Received::Lost
        ]
    );
    assert!(
        h.sibling.borrow().received.is_empty(),
        "orphan release is discarded"
    );
    h.app.shutdown().await;
}

#[tokio::test]
async fn capture_uses_owner_origin_from_latest_frame() {
    let mut h = Harness::new().await;
    h.mouse(Type::Press, 3, 5);
    h.tree.borrow_mut().owner_origin = RelativePoint { row: 5, col: 7 };
    h.redraw();
    h.mouse(Type::Drag, 3, 17);
    h.mouse(Type::Release, 3, 17);
    assert_eq!(
        h.owner.borrow().received,
        [
            received(Type::Press, 1, 1),
            received(Type::Drag, -2, 10),
            received(Type::Release, -2, 10),
        ]
    );
    assert!(h.sibling.borrow().received.is_empty());
    h.app.shutdown().await;
}
