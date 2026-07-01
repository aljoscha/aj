//! Port of upstream `examples/view.zig`: two oversized ASCII world maps drawn
//! through a `View` and panned/scrolled across a window. `z` swaps the map
//! zoom, `m` toggles a fixed-size mini view, the arrow keys scroll (with Ctrl
//! for a big step and Shift to jump a full map width/height), Ctrl-C quits.
//!
//! Upstream computes the two maps at comptime from multi-line string literals.
//! We keep the same authoritative data in sibling `.txt` files and `include_str!`
//! them, deriving the width (first line length) and height (line count) exactly
//! as upstream does with `indexOf('\n')` and `count('\n')`.

use std::error::Error;
use std::time::Duration;

use vaxis::Winsize;
use vaxis::cell::{Segment, Style, Underline};
use vaxis::event::Event as VxEvent;
use vaxis::event_loop::{FromEvent, Loop};
use vaxis::key::{Key, KittyFlags, Modifiers};
use vaxis::tty::{PosixTty, Tty};
use vaxis::vaxis::{Options, Vaxis};
use vaxis::widgets::view::{Config, DrawOptions, View};
use vaxis::window::{BorderOptions, BorderWhere, ChildOptions, PrintOptions, Wrap};

const LG_MAP: &str = include_str!("view_lg_map.txt");
const SM_MAP: &str = include_str!("view_sm_map.txt");

enum Event {
    KeyPress(Key),
    Winsize(Winsize),
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

/// Map dimensions in cells: width is the first line's length, height the line
/// count, matching upstream's comptime `indexOf('\n')` / `count('\n')`.
fn map_dims(map: &str) -> (u16, u16) {
    let width = u16::try_from(map.find('\n').unwrap_or(0)).unwrap_or(0);
    let height = u16::try_from(map.matches('\n').count()).unwrap_or(0);
    (width, height)
}

/// The map with newlines removed, ready to print with grapheme wrap into a
/// width-by-height `View` (upstream's `mem.replace(map, "\n", "")`).
fn stripped(map: &str) -> String {
    map.chars().filter(|&c| c != '\n').collect()
}

fn main() -> Result<(), Box<dyn Error>> {
    let (lg_w, lg_h) = map_dims(LG_MAP);
    let (sm_w, sm_h) = map_dims(SM_MAP);

    let mut map_width = lg_w;
    let mut map_height = lg_h;
    let mut use_sm_map = false;
    let mut use_mini_view = false;

    let mut x: u16 = 0;
    let mut y: u16 = 0;

    let mut tty = PosixTty::new()?;
    let mut vx = Vaxis::new(Options {
        kitty_keyboard_flags: KittyFlags::default() | KittyFlags::REPORT_EVENTS,
        ..Options::default()
    });
    let mut input_loop = Loop::<Event>::init(&tty, &vx)?;
    input_loop.install_resize_handler(&tty)?;
    input_loop.start();

    vx.enter_alt_screen(&mut tty.writer())?;
    vx.query_terminal(&mut tty.writer(), Duration::from_secs(20))?;

    // Lay each map into its own oversized off-screen view once.
    let lg_map_view = View::new(Config {
        width: lg_w,
        height: lg_h,
    });
    lg_map_view.print_segment(
        Segment {
            text: stripped(LG_MAP),
            ..Segment::default()
        },
        PrintOptions {
            wrap: Wrap::Grapheme,
            ..PrintOptions::default()
        },
    );
    let sm_map_view = View::new(Config {
        width: sm_w,
        height: sm_h,
    });
    sm_map_view.print_segment(
        Segment {
            text: stripped(SM_MAP),
            ..Segment::default()
        },
        PrintOptions {
            wrap: Wrap::Grapheme,
            ..PrintOptions::default()
        },
    );

    'main: loop {
        match input_loop.next_event() {
            Event::KeyPress(key) => {
                if key.matches(u32::from('c'), Modifiers::CTRL) {
                    break 'main;
                }
                // Scroll one cell.
                if key.matches(Key::LEFT, Modifiers::empty()) {
                    x = x.saturating_sub(1);
                }
                if key.matches(Key::RIGHT, Modifiers::empty()) {
                    x = x.saturating_add(1);
                }
                if key.matches(Key::UP, Modifiers::empty()) {
                    y = y.saturating_sub(1);
                }
                if key.matches(Key::DOWN, Modifiers::empty()) {
                    y = y.saturating_add(1);
                }
                // Quick scroll.
                if key.matches(Key::LEFT, Modifiers::CTRL) {
                    x = x.saturating_sub(30);
                }
                if key.matches(Key::RIGHT, Modifiers::CTRL) {
                    x = x.saturating_add(30);
                }
                if key.matches(Key::UP, Modifiers::CTRL) {
                    y = y.saturating_sub(10);
                }
                if key.matches(Key::DOWN, Modifiers::CTRL) {
                    y = y.saturating_add(10);
                }
                // Jump a full map width/height.
                if key.matches(Key::LEFT, Modifiers::SHIFT) {
                    x = x.saturating_sub(map_width);
                }
                if key.matches(Key::RIGHT, Modifiers::SHIFT) {
                    x = x.saturating_add(map_width);
                }
                if key.matches(Key::UP, Modifiers::SHIFT) {
                    y = y.saturating_sub(map_height);
                }
                if key.matches(Key::DOWN, Modifiers::SHIFT) {
                    y = y.saturating_add(map_height);
                }
                // Swap zoom.
                if key.matches(u32::from('z'), Modifiers::empty()) {
                    use_sm_map = !use_sm_map;
                    (map_width, map_height) = if use_sm_map {
                        (sm_w, sm_h)
                    } else {
                        (lg_w, lg_h)
                    };
                }
                // Toggle the fixed-size mini view.
                if key.matches(u32::from('m'), Modifiers::empty()) {
                    use_mini_view = !use_mini_view;
                }
            }
            Event::Winsize(ws) => vx.resize(&mut tty.writer(), ws)?,
        }

        let win = vx.window();
        win.clear();

        let controls_win = win.child(ChildOptions {
            height: Some(1),
            ..ChildOptions::default()
        });
        let controls: Vec<Segment> = if win.width >= 112 {
            vec![
                Segment {
                    text: "Controls:".to_string(),
                    style: Style {
                        bold: true,
                        ul_style: Underline::Single,
                        ..Style::default()
                    },
                    ..Segment::default()
                },
                Segment {
                    text: " Exit: ctrl + c | Scroll: dpad | Quick Scroll: ctrl + dpad | Goto Side: shift + dpad | Zoom: z | Mini: m".to_string(),
                    ..Segment::default()
                },
            ]
        } else if win.width >= 25 {
            vec![
                Segment {
                    text: "Controls:".to_string(),
                    style: Style {
                        bold: true,
                        ul_style: Underline::Single,
                        ..Style::default()
                    },
                    ..Segment::default()
                },
                Segment {
                    text: " Win too small!".to_string(),
                    ..Segment::default()
                },
            ]
        } else {
            vec![Segment::default()]
        };
        controls_win.print(
            &controls,
            PrintOptions {
                wrap: Wrap::None,
                ..PrintOptions::default()
            },
        );

        let map_win = if use_mini_view {
            win.child(ChildOptions {
                y_off: i32::from(controls_win.height),
                border: BorderOptions {
                    location: BorderWhere::Top,
                    ..BorderOptions::default()
                },
                width: Some(45),
                height: Some(15),
                ..ChildOptions::default()
            })
        } else {
            win.child(ChildOptions {
                y_off: i32::from(controls_win.height),
                border: BorderOptions {
                    location: BorderWhere::Top,
                    ..BorderOptions::default()
                },
                ..ChildOptions::default()
            })
        };

        // Clamp so the visible rectangle stays inside the map.
        x = x.min(map_width.saturating_sub(map_win.width));
        y = y.min(map_height.saturating_sub(map_win.height));

        let active = if use_sm_map {
            &sm_map_view
        } else {
            &lg_map_view
        };
        active.draw(map_win, DrawOptions { x_off: x, y_off: y });

        if use_mini_view {
            win.print_segment(
                Segment {
                    text: "This is a mini portion of the View.".to_string(),
                    ..Segment::default()
                },
                PrintOptions {
                    row_offset: 16,
                    col_offset: 5,
                    wrap: Wrap::Word,
                    ..PrintOptions::default()
                },
            );
        }

        vx.render(&mut tty.writer())?;
    }

    input_loop.signal_stop();
    let _ = vx.device_status_report(&mut tty.writer());
    input_loop.stop();
    vx.reset_state(&mut tty.writer())?;
    Ok(())
}
