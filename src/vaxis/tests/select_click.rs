#![cfg(unix)]

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;

use vaxis::key::{Key, Modifiers};
use vaxis::mouse::{Button, Mouse, Type};
use vaxis::tty::TestTty;
use vaxis::vaxis::{Options as VaxisOptions, Vaxis};
use vaxis::vxfw::{
    AsyncApp, DrawContext, Event, EventContext, FilterableSelect, MaxSize, Options, RelativePoint,
    SelectItem, SelectStyles, Size, SubSurface, Surface, Widget, WidgetRef, draw_widget,
    to_widget_ref,
};

struct PickerRoot(Rc<RefCell<FilterableSelect>>);

impl Widget for PickerRoot {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let mut surface = Surface::with_size(ctx.max.size());
        surface.children.push(SubSurface {
            origin: RelativePoint { row: 3, col: 4 },
            surface: draw_widget(
                &to_widget_ref(Rc::clone(&self.0)),
                &ctx.with_constraints(
                    Size::default(),
                    MaxSize {
                        width: Some(24),
                        height: Some(7),
                    },
                ),
            ),
            z_index: 0,
        });
        surface
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        if matches!(event, Event::Init) {
            ctx.request_focus(self.0.borrow().focus_target());
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

struct Harness {
    app: AsyncApp,
    root: WidgetRef,
    select: Rc<RefCell<FilterableSelect>>,
    confirmed: Rc<RefCell<Vec<String>>>,
    _write_fd: OwnedFd,
}

impl Harness {
    async fn new() -> Self {
        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        nix::unistd::write(&write_fd, b"\x1b[?c").unwrap();
        let select = Rc::new(RefCell::new(FilterableSelect::new(
            rows(),
            SelectStyles::default(),
        )));
        select.borrow_mut().set_literal_search(true);
        select.borrow_mut().set_show_scrollbar(true);
        let confirmed = Rc::new(RefCell::new(Vec::new()));
        let results = Rc::clone(&confirmed);
        select.borrow_mut().on_confirm = Some(Box::new(move |_, item| {
            results.borrow_mut().push(item.filter_key.clone());
        }));
        let root = to_widget_ref(Rc::new(RefCell::new(PickerRoot(Rc::clone(&select)))));
        let mut app = AsyncApp::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
            read_fd,
        );
        app.init(Rc::clone(&root), Options::default())
            .await
            .unwrap();
        Self {
            app,
            root,
            select,
            confirmed,
            _write_fd: write_fd,
        }
    }

    fn draw(&mut self) {
        self.app.render(&self.root).unwrap();
    }

    fn mouse(&mut self, button: Button, kind: Type, row: i16, col: i16) {
        self.app.handle_input(Event::Mouse(Mouse {
            row,
            col,
            button,
            kind,
            mods: Default::default(),
            xoffset: 0,
            yoffset: 0,
        }));
    }

    fn click(&mut self, row: i16, col: i16) {
        self.mouse(Button::Left, Type::Press, row, col);
        self.draw();
        self.mouse(Button::Left, Type::Release, row, col);
    }

    fn key(&mut self, codepoint: u32, mods: Modifiers, text: Option<&str>) {
        self.app.handle_input(Event::KeyPress(Key {
            codepoint,
            mods,
            text: text.map(Into::into),
            ..Key::default()
        }));
        self.draw();
    }

    fn selected(&self) -> String {
        self.select.borrow().selected().unwrap().filter_key
    }
}

fn rows() -> Vec<SelectItem> {
    (0..20)
        .map(|i| SelectItem::new(format!("row{i:02}"), format!("row{i:02}")))
        .collect()
}

#[tokio::test]
async fn clicks_select_drawn_rows_after_scrolling_and_filtering_without_confirming_or_taking_focus()
{
    let mut h = Harness::new().await;
    for _ in 0..3 {
        h.mouse(Button::WheelDown, Type::Press, 6, 8);
    }
    h.draw();
    // The picker is offset from the root. Its query and gap take two rows,
    // so row 6 is the second visible item, now row04 after three wheel ticks.
    h.click(6, 24); // Row padding is part of the row's hit area.
    assert_eq!(h.selected(), "row04");
    assert!(h.confirmed.borrow().is_empty());
    h.click(7, 27); // The scrollbar is not a row.
    h.mouse(Button::Right, Type::Press, 5, 8);
    assert_eq!(h.selected(), "row04");

    let mut incoming = vec![SelectItem::new("newest", "newest")];
    incoming.extend(rows());
    h.select.borrow().set_ranked_items(incoming);
    h.draw();
    assert_eq!(h.selected(), "row04", "a click stops automatic following");
    h.click(5, 8);
    assert_eq!(h.selected(), "row03", "clicking did not move the viewport");

    h.key(u32::from('1'), Modifiers::empty(), Some("1"));
    assert_eq!(h.select.borrow().query(), "1", "focus stayed in the filter");
    h.click(6, 8);
    assert_eq!(h.selected(), "row10", "click uses filtered order");
    assert!(h.confirmed.borrow().is_empty());
    h.key(Key::ENTER, Modifiers::empty(), None);
    assert_eq!(&*h.confirmed.borrow(), &["row10"]);

    h.key(u32::from('9'), Modifiers::empty(), Some("9"));
    assert_eq!(h.selected(), "row19");
    h.click(8, 8); // Below the sole matching row.
    assert_eq!(h.selected(), "row19");
    assert_eq!(h.confirmed.borrow().len(), 1);
}

#[tokio::test]
async fn clicks_ignore_rows_replaced_since_the_last_paint() {
    let mut h = Harness::new().await;
    h.select.borrow().set_items(vec![
        SelectItem::new("old-a", "a"),
        SelectItem::new("old-b", "b"),
    ]);
    h.draw();
    h.select.borrow().set_items(vec![
        SelectItem::new("new-a", "x"),
        SelectItem::new("new-b", "y"),
    ]);
    h.mouse(Button::Left, Type::Press, 6, 8);
    assert_eq!(
        h.selected(),
        "x",
        "the painted old-b must not select unseen new-b"
    );
    h.draw();
    h.click(6, 8);
    assert_eq!(h.selected(), "y");
}
