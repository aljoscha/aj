//! Integration tests for `#[derive(TableRow)]`.
//!
//! These live in `tests/` (a separate crate that depends on `vaxis` as an
//! external crate) so the derive's fully-qualified `::vaxis::` paths resolve.
//! The crate cannot use `extern crate self as vaxis` internally because it
//! already has a `vaxis` runtime module.

use std::cell::RefCell;
use std::fmt;

use vaxis::TableRow;
use vaxis::screen::Screen;
use vaxis::widgets::table::{HorizontalAlignment, TableContext, WidthStyle, draw_table};
use vaxis::window::Window;

#[derive(Debug)]
enum Role {
    Admin,
    User,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Role::Admin => write!(f, "Admin"),
            Role::User => write!(f, "User"),
        }
    }
}

#[derive(TableRow)]
struct Person {
    #[table(rename = "Full Name")]
    name: String,
    age: u32,
    role: Role,
    email: Option<String>,
}

#[derive(TableRow)]
struct WithSkip {
    visible: String,
    #[table(skip)]
    #[allow(dead_code)]
    hidden: u64,
    also_visible: i32,
}

fn header_strings<R: TableRow>() -> Vec<String> {
    R::headers().iter().map(|c| c.to_string()).collect()
}

#[test]
fn headers_use_field_names_and_rename() {
    assert_eq!(
        header_strings::<Person>(),
        vec!["Full Name", "age", "role", "email"]
    );
    assert_eq!(Person::column_count(), 4);
}

#[test]
fn skip_drops_column() {
    assert_eq!(
        header_strings::<WithSkip>(),
        vec!["visible", "also_visible"]
    );
    assert_eq!(WithSkip::column_count(), 2);

    let row = WithSkip {
        visible: "v".to_string(),
        hidden: 99,
        also_visible: -3,
    };
    assert_eq!(row.cell(0).as_ref(), "v");
    // Column 1 skips `hidden` and maps to `also_visible`.
    assert_eq!(row.cell(1).as_ref(), "-3");
    // Out-of-range columns render empty.
    assert_eq!(row.cell(2).as_ref(), "");
}

#[test]
fn cell_formats_by_field_type() {
    let p = Person {
        name: "Ada".to_string(),
        age: 30,
        role: Role::Admin,
        email: Some("ada@x.io".to_string()),
    };
    // String direct.
    assert_eq!(p.cell(0).as_ref(), "Ada");
    // Integer via Display.
    assert_eq!(p.cell(1).as_ref(), "30");
    // Enum via Display (upstream used @tagName).
    assert_eq!(p.cell(2).as_ref(), "Admin");
    // Option Some unwraps to the inner value.
    assert_eq!(p.cell(3).as_ref(), "ada@x.io");

    let q = Person {
        name: "Bo".to_string(),
        age: 5,
        role: Role::User,
        email: None,
    };
    assert_eq!(q.cell(2).as_ref(), "User");
    // Option None renders as "-".
    assert_eq!(q.cell(3).as_ref(), "-");
}

#[test]
fn derived_row_draws_into_a_window() {
    let screen = RefCell::new(Screen::new(vaxis::Winsize {
        rows: 6,
        cols: 40,
        x_pixel: 0,
        y_pixel: 0,
    }));
    let win = Window {
        x_off: 0,
        y_off: 0,
        parent_x_off: 0,
        parent_y_off: 0,
        width: 40,
        height: 6,
        screen: &screen,
    };
    let people = vec![Person {
        name: "Ada".to_string(),
        age: 30,
        role: Role::Admin,
        email: None,
    }];
    let mut ctx = TableContext {
        col_width: WidthStyle::StaticAll(10),
        header_align: HorizontalAlignment::Left,
        cell_x_off: 0,
        ..TableContext::default()
    };
    draw_table(&win, &people, &mut ctx).unwrap();

    // Header "Full Name" starts at col 0, row 0; the None email renders "-".
    let s = screen.borrow();
    assert_eq!(s.read_cell(0, 0).unwrap().char.grapheme(), "F");
    assert_eq!(s.read_cell(0, 1).unwrap().char.grapheme(), "A");
    assert_eq!(s.read_cell(30, 1).unwrap().char.grapheme(), "-");
}
