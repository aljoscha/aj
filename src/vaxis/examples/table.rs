//! Port of upstream `examples/table.zig`: a demo of the `Table` widget over a
//! slice of `User` records, with a top branding bar, a command bar, vim-style
//! navigation, row selection, and an expanding active row.
//!
//! `Ctrl-W` toggles "moving" (section focus) mode, the arrows/hjkl move the
//! active cell, Space toggles row selection, Enter expands the active row, and
//! `:`/`/`/`g`/`G` drop into the command bar (`:q` quits, `G`/`gg<n>` jump).
//!
//! The `User` columns come from `#[derive(TableRow)]`, replacing upstream's
//! comptime struct reflection. The two block-glyph logos live in sibling
//! `.txt` files, included byte-for-byte.

use std::error::Error;
use std::time::Duration;

use vaxis::cell::{Cell, Color, Segment, Style};
use vaxis::event::Event as VxEvent;
use vaxis::event_loop::{FromEvent, Loop};
use vaxis::key::{Key, KittyFlags, Modifiers};
use vaxis::tty::{PosixTty, Tty};
use vaxis::vaxis::{Options, Vaxis};
use vaxis::widgets::alignment;
use vaxis::widgets::table::{
    ActiveContentFn, ColumnIndexes, HeaderNames, TableContext, draw_table,
};
use vaxis::widgets::text_input::{Event as InputEvent, TextInput};
use vaxis::window::{ChildOptions, PrintOptions, Wrap};
use vaxis::{TableRow, Winsize};

const TITLE_LOGO: &str = include_str!("table_title_logo.txt");
const CONTENT_LOGO: &str = include_str!("table_content_logo.txt");

/// Which of the three vertical sections currently has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveSection {
    Top,
    Mid,
    Btm,
}

/// A row of the demo table. Columns and headers derive from the fields.
#[derive(TableRow)]
struct User {
    first: &'static str,
    last: &'static str,
    user: &'static str,
    email: Option<&'static str>,
    phone: Option<&'static str>,
}

enum Event {
    KeyPress(Key),
    Winsize(Winsize),
    /// Posted by the app to force a redraw when the active row expands. Never
    /// produced by the parser.
    TableUpd,
}

impl FromEvent for Event {
    fn from_event(event: VxEvent) -> Option<Self> {
        match event {
            VxEvent::KeyPress(key) => Some(Event::KeyPress(key)),
            VxEvent::Winsize(ws) => Some(Event::Winsize(ws)),
            _ => None,
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let users = users();

    let mut tty = PosixTty::new()?;
    let mut vx = Vaxis::new(Options {
        kitty_keyboard_flags: KittyFlags::default() | KittyFlags::REPORT_EVENTS,
        ..Options::default()
    });
    let mut input_loop = Loop::<Event>::init(&tty, &vx)?;
    input_loop.install_resize_handler(&tty)?;
    input_loop.start();

    vx.enter_alt_screen(&mut tty.writer())?;
    vx.query_terminal(&mut tty.writer(), Duration::from_millis(250))?;

    let mut cmd_input = TextInput::new();

    let active_bg = Color::Rgb([64, 128, 255]);
    let selected_bg = Color::Rgb([32, 64, 255]);
    let other_bg = Color::Rgb([32, 32, 48]);

    let mut demo_tbl = TableContext {
        active_bg,
        active_fg: Color::Rgb([0, 0, 0]),
        row_bg_1: Color::Rgb([8, 8, 8]),
        selected_bg,
        header_names: HeaderNames::Custom(vec![
            "First".to_string(),
            "Last".to_string(),
            "Username".to_string(),
            "Phone#".to_string(),
            "Email".to_string(),
        ]),
        col_indexes: ColumnIndexes::ByIdx(vec![0, 1, 2, 4, 3]),
        ..TableContext::default()
    };

    let mut active = ActiveSection::Mid;
    let mut moving = false;
    let mut see_content = false;

    'main: loop {
        match input_loop.next_event() {
            Event::KeyPress(key) => {
                'key: {
                    if key.matches(u32::from('c'), Modifiers::CTRL) {
                        break 'main;
                    }
                    if key.matches(u32::from('l'), Modifiers::CTRL) {
                        vx.queue_refresh();
                        break 'key;
                    }
                    if key.matches(u32::from('w'), Modifiers::CTRL) {
                        moving = !moving;
                        break 'key;
                    }
                    if active != ActiveSection::Btm
                        && key.matches_any(
                            &[
                                u32::from(':'),
                                u32::from('/'),
                                u32::from('g'),
                                u32::from('G'),
                            ],
                            Modifiers::empty(),
                        )
                    {
                        active = ActiveSection::Btm;
                        cmd_input.clear_and_free();
                        cmd_input.update(&InputEvent::KeyPress(key.clone()));
                        break 'key;
                    }

                    match active {
                        ActiveSection::Top => {
                            if key.matches_any(&[Key::DOWN, u32::from('j')], Modifiers::empty())
                                && moving
                            {
                                active = ActiveSection::Mid;
                            }
                        }
                        ActiveSection::Mid => 'mid: {
                            if moving {
                                if key.matches_any(&[Key::UP, u32::from('k')], Modifiers::empty()) {
                                    active = ActiveSection::Top;
                                }
                                if key.matches_any(&[Key::DOWN, u32::from('j')], Modifiers::empty())
                                {
                                    active = ActiveSection::Btm;
                                }
                                break 'mid;
                            }
                            // Move the active cell.
                            if key.matches_any(&[Key::UP, u32::from('k')], Modifiers::empty()) {
                                demo_tbl.row = demo_tbl.row.saturating_sub(1);
                            }
                            if key.matches_any(&[Key::DOWN, u32::from('j')], Modifiers::empty()) {
                                demo_tbl.row = demo_tbl.row.saturating_add(1);
                            }
                            if key.matches_any(&[Key::LEFT, u32::from('h')], Modifiers::empty()) {
                                demo_tbl.col = demo_tbl.col.saturating_sub(1);
                            }
                            if key.matches_any(&[Key::RIGHT, u32::from('l')], Modifiers::empty()) {
                                demo_tbl.col = demo_tbl.col.saturating_add(1);
                            }
                            // Toggle selection of the active row.
                            if key.matches(Key::SPACE, Modifiers::empty()) {
                                let current = demo_tbl.row;
                                let rows = demo_tbl.sel_rows.get_or_insert_with(Vec::new);
                                if let Some(idx) = rows.iter().position(|&r| r == current) {
                                    rows.remove(idx);
                                } else {
                                    rows.push(current);
                                }
                            }
                            // Toggle the expanding active-row content.
                            if key.matches(Key::ENTER, Modifiers::empty())
                                || key.matches(u32::from('j'), Modifiers::CTRL)
                            {
                                see_content = !see_content;
                            }
                        }
                        ActiveSection::Btm => {
                            if key.matches_any(&[Key::UP, u32::from('k')], Modifiers::empty())
                                && moving
                            {
                                active = ActiveSection::Mid;
                            } else if key.match_exact(Key::ENTER, Modifiers::empty())
                                || key.match_exact(u32::from('j'), Modifiers::CTRL)
                            {
                                let cmd = cmd_input.to_owned_slice();
                                if cmd == ":q" || cmd == ":quit" || cmd == ":exit" {
                                    break 'main;
                                }
                                if cmd == "G" {
                                    demo_tbl.row = u16::try_from(users.len().saturating_sub(1))
                                        .unwrap_or(u16::MAX);
                                    active = ActiveSection::Mid;
                                }
                                if let Some(rest) = cmd.strip_prefix("gg") {
                                    demo_tbl.row = rest.parse::<u16>().unwrap_or(0);
                                    active = ActiveSection::Mid;
                                }
                            } else {
                                cmd_input.update(&InputEvent::KeyPress(key.clone()));
                            }
                        }
                    }
                    moving = false;
                }
            }
            Event::Winsize(ws) => vx.resize(&mut tty.writer(), ws)?,
            Event::TableUpd => {}
        }

        // Expanding active-row content. Rebuilt each frame from the current row.
        if see_content {
            let row_label = format!("Row #: {}", demo_tbl.row);
            let bg = demo_tbl.active_bg;
            let content: ActiveContentFn = Box::new(move |win| {
                win.height = 5;
                let see_win = win.child(ChildOptions {
                    x_off: 0,
                    y_off: 1,
                    width: Some(win.width),
                    height: Some(4),
                    ..ChildOptions::default()
                });
                see_win.fill(Cell {
                    style: Style {
                        bg,
                        ..Style::default()
                    },
                    ..Cell::default()
                });
                let segs = [
                    Segment {
                        text: row_label.clone(),
                        style: Style {
                            bg,
                            ..Style::default()
                        },
                        ..Segment::default()
                    },
                    Segment {
                        text: CONTENT_LOGO.to_string(),
                        style: Style {
                            bg,
                            ..Style::default()
                        },
                        ..Segment::default()
                    },
                ];
                see_win.print(&segs, PrintOptions::default());
                see_win.height
            });
            demo_tbl.active_content_fn = Some(content);
            input_loop.post_event(Event::TableUpd);
        } else {
            demo_tbl.active_content_fn = None;
        }

        let win = vx.window();
        win.clear();

        // Top branding bar.
        let top_div: u16 = 6;
        let top_bar = win.child(ChildOptions {
            x_off: 0,
            y_off: 0,
            width: Some(win.width),
            height: Some(win.height / top_div),
            ..ChildOptions::default()
        });
        let title_bg = if active == ActiveSection::Top {
            selected_bg
        } else {
            other_bg
        };
        top_bar.fill(Cell {
            style: Style {
                bg: title_bg,
                ..Style::default()
            },
            ..Cell::default()
        });
        let title_segs = [
            Segment {
                text: TITLE_LOGO.to_string(),
                style: Style {
                    bg: title_bg,
                    ..Style::default()
                },
                ..Segment::default()
            },
            Segment {
                text: "===A Demo of the the Vaxis Table Widget!===".to_string(),
                style: Style {
                    bg: title_bg,
                    ..Style::default()
                },
                ..Segment::default()
            },
            Segment {
                text: "(All data is non-sensical & LLM generated.)".to_string(),
                style: Style {
                    bg: title_bg,
                    ..Style::default()
                },
                ..Segment::default()
            },
        ];
        let logo_bar = alignment::center(
            top_bar,
            44,
            top_bar.height.saturating_sub(top_bar.height / 3),
        );
        logo_bar.print(
            &title_segs,
            PrintOptions {
                wrap: Wrap::Word,
                ..PrintOptions::default()
            },
        );

        // Middle: the table.
        let middle_bar = win.child(ChildOptions {
            x_off: 0,
            y_off: i32::from(win.height / top_div),
            width: Some(win.width),
            height: Some(win.height.saturating_sub(top_bar.height.saturating_add(1))),
            ..ChildOptions::default()
        });
        if !users.is_empty() {
            demo_tbl.active = active == ActiveSection::Mid;
            draw_table(&middle_bar, &users, &mut demo_tbl)?;
        }

        // Bottom: the command bar.
        let bottom_bar = win.child(ChildOptions {
            x_off: 0,
            y_off: i32::from(win.height.saturating_sub(1)),
            width: Some(win.width),
            height: Some(1),
            ..ChildOptions::default()
        });
        if active == ActiveSection::Btm {
            bottom_bar.fill(Cell {
                style: Style {
                    bg: active_bg,
                    ..Style::default()
                },
                ..Cell::default()
            });
        }
        cmd_input.draw(bottom_bar);

        vx.render(&mut tty.writer())?;
    }

    input_loop.signal_stop();
    let _ = vx.device_status_report(&mut tty.writer());
    input_loop.stop();
    vx.reset_state(&mut tty.writer())?;
    Ok(())
}

/// The demo data. LLM-generated, non-sensical, matching upstream verbatim.
fn users() -> Vec<User> {
    vec![
        User {
            first: "Nancy",
            last: "Dudley",
            user: "angela73",
            email: Some("brian47@rodriguez.biz"),
            phone: None,
        },
        User {
            first: "Emily",
            last: "Thornton",
            user: "mrogers",
            email: None,
            phone: Some("(558)888-8604x094"),
        },
        User {
            first: "Kyle",
            last: "Huff",
            user: "xsmith",
            email: None,
            phone: Some("301.127.0801x12398"),
        },
        User {
            first: "Christine",
            last: "Dodson",
            user: "amandabradley",
            email: Some("cheryl21@sullivan.com"),
            phone: None,
        },
        User {
            first: "Nathaniel",
            last: "Kennedy",
            user: "nrobinson",
            email: None,
            phone: None,
        },
        User {
            first: "Laura",
            last: "Leon",
            user: "dawnjones",
            email: Some("fjenkins@patel.com"),
            phone: Some("1833013180"),
        },
        User {
            first: "Patrick",
            last: "Landry",
            user: "michaelhutchinson",
            email: Some("daniel17@medina-wallace.net"),
            phone: Some("+1-634-486-6444x964"),
        },
        User {
            first: "Tammy",
            last: "Hall",
            user: "jamessmith",
            email: None,
            phone: Some("(926)810-3385x22059"),
        },
        User {
            first: "Stephanie",
            last: "Anderson",
            user: "wgillespie",
            email: Some("campbelljaime@yahoo.com"),
            phone: None,
        },
        User {
            first: "Jennifer",
            last: "Williams",
            user: "shawn60",
            email: None,
            phone: Some("611-385-4771x97523"),
        },
        User {
            first: "Elizabeth",
            last: "Ortiz",
            user: "jennifer76",
            email: Some("johnbradley@delgado.info"),
            phone: None,
        },
        User {
            first: "Stacy",
            last: "Mays",
            user: "scottgonzalez",
            email: Some("kramermatthew@gmail.com"),
            phone: None,
        },
        User {
            first: "Jennifer",
            last: "Smith",
            user: "joseph75",
            email: Some("masseyalexander@hill-moore.net"),
            phone: None,
        },
        User {
            first: "Gary",
            last: "Hammond",
            user: "brittany26",
            email: None,
            phone: None,
        },
        User {
            first: "Lisa",
            last: "Johnson",
            user: "tina28",
            email: None,
            phone: Some("850-606-2978x1081"),
        },
        User {
            first: "Zachary",
            last: "Hopkins",
            user: "vargasmichael",
            email: None,
            phone: None,
        },
        User {
            first: "Joshua",
            last: "Kidd",
            user: "ghanna",
            email: Some("jbrown@yahoo.com"),
            phone: None,
        },
        User {
            first: "Dawn",
            last: "Jones",
            user: "alisonlindsey",
            email: None,
            phone: None,
        },
        User {
            first: "Monica",
            last: "Berry",
            user: "barbara40",
            email: Some("michael00@hotmail.com"),
            phone: Some("(295)346-6453x343"),
        },
        User {
            first: "Shannon",
            last: "Roberts",
            user: "krystal37",
            email: None,
            phone: Some("980-920-9386x454"),
        },
        User {
            first: "Thomas",
            last: "Mitchell",
            user: "williamscorey",
            email: Some("richardduncan@roberts.com"),
            phone: None,
        },
        User {
            first: "Nicole",
            last: "Shaffer",
            user: "rogerstroy",
            email: None,
            phone: Some("(570)128-5662"),
        },
        User {
            first: "Edward",
            last: "Bennett",
            user: "andersonchristina",
            email: None,
            phone: None,
        },
        User {
            first: "Duane",
            last: "Howard",
            user: "pcarpenter",
            email: Some("griffithwayne@parker.net"),
            phone: None,
        },
        User {
            first: "Mary",
            last: "Brown",
            user: "kimberlyfrost",
            email: Some("perezsara@anderson-andrews.net"),
            phone: None,
        },
        User {
            first: "Pamela",
            last: "Sloan",
            user: "kvelez",
            email: Some("huynhlacey@moore-bell.biz"),
            phone: Some("001-359-125-1393x8716"),
        },
        User {
            first: "Timothy",
            last: "Charles",
            user: "anthony04",
            email: Some("morrissara@hawkins.info"),
            phone: Some("+1-619-369-9572"),
        },
        User {
            first: "Sydney",
            last: "Torres",
            user: "scott42",
            email: Some("asnyder@mitchell.net"),
            phone: None,
        },
        User {
            first: "John",
            last: "Jones",
            user: "anthonymoore",
            email: None,
            phone: Some("701.236.0571x99622"),
        },
        User {
            first: "Erik",
            last: "Johnson",
            user: "allisonsanders",
            email: None,
            phone: None,
        },
        User {
            first: "Donna",
            last: "Kirk",
            user: "laurie81",
            email: None,
            phone: None,
        },
        User {
            first: "Karina",
            last: "White",
            user: "uperez",
            email: None,
            phone: None,
        },
        User {
            first: "Jesse",
            last: "Schwartz",
            user: "ryan60",
            email: Some("latoyawilliams@gmail.com"),
            phone: None,
        },
        User {
            first: "Cindy",
            last: "Romero",
            user: "christopher78",
            email: Some("faulknerchristina@gmail.com"),
            phone: Some("780.288.2319x583"),
        },
        User {
            first: "Tyler",
            last: "Sanders",
            user: "bennettjessica",
            email: None,
            phone: Some("1966269423"),
        },
        User {
            first: "Pamela",
            last: "Carter",
            user: "zsnyder",
            email: None,
            phone: Some("125-062-9130x58413"),
        },
    ]
}
