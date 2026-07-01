//! Port of upstream `examples/main.zig` (renamed `hello` since `main` reads as
//! the entry point). Minimal hello-world: a centered message whose color cycles
//! on every keypress, with `j`/`k` shrinking/growing the scaled text when the
//! terminal supports it.

use std::error::Error;
use std::time::Duration;

use vaxis::Winsize;
use vaxis::cell::{Cell, Character, Color, Scale, Style};
use vaxis::event::Event as VxEvent;
use vaxis::event_loop::{FromEvent, Loop};
use vaxis::key::{Key, Modifiers};
use vaxis::tty::{PosixTty, Tty};
use vaxis::vaxis::{Options, Vaxis};
use vaxis::window::ChildOptions;

/// The events this app handles. Mirrors upstream's `union(enum)` with the
/// `focus_in` and `foo` variants that never fire, kept for parity.
enum Event {
    KeyPress(Key),
    Winsize(Winsize),
    FocusIn,
    #[allow(dead_code)]
    Foo(u8),
}

impl FromEvent for Event {
    fn from_event(event: VxEvent) -> Option<Self> {
        match event {
            VxEvent::KeyPress(key) => Some(Event::KeyPress(key)),
            VxEvent::Winsize(ws) => Some(Event::Winsize(ws)),
            VxEvent::FocusIn => Some(Event::FocusIn),
            _ => None,
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    // `PosixTty::new` installs the panic hook that resets the terminal, so a
    // panic leaves the screen usable (upstream's `panic_handler`).
    let mut tty = PosixTty::new()?;
    let mut vx = Vaxis::new(Options::default());
    let mut input_loop = Loop::<Event>::init(&tty, &vx)?;
    input_loop.install_resize_handler(&tty)?;
    input_loop.start();

    vx.enter_alt_screen(&mut tty.writer())?;
    vx.query_terminal(&mut tty.writer(), Duration::from_secs(1))?;

    let mut color_idx: u8 = 0;
    let msg = "Hello, world!";
    let mut scale: u8 = 1;

    'main: loop {
        match input_loop.next_event() {
            Event::KeyPress(key) => {
                color_idx = color_idx.wrapping_add(1);
                if key.codepoint == u32::from('c') && key.mods.contains(Modifiers::CTRL) {
                    break 'main;
                }
                if key.matches(u32::from('j'), Modifiers::empty())
                    && vx.caps.scaled_text
                    && scale > 1
                {
                    scale -= 1;
                }
                if key.matches(u32::from('k'), Modifiers::empty())
                    && vx.caps.scaled_text
                    && scale < 7
                {
                    scale += 1;
                }
            }
            Event::Winsize(ws) => vx.resize(&mut tty.writer(), ws)?,
            _ => {}
        }

        let win = vx.window();
        win.clear();

        let msg_len = u16::try_from(msg.len()).unwrap_or(u16::MAX);
        // `.expand`: no explicit width/height, so the child fills the parent.
        let child = win.child(ChildOptions {
            x_off: i32::from(win.width / 2) - i32::from(msg_len / 2),
            y_off: i32::from(win.height / 2),
            ..ChildOptions::default()
        });

        for (i, _) in msg.bytes().enumerate() {
            let grapheme = &msg[i..i + 1];
            let col = u16::try_from(i).unwrap_or(u16::MAX);
            let scaled = Cell {
                char: Character::new(grapheme, 1),
                style: Style {
                    fg: Color::Index(color_idx),
                    ..Style::default()
                },
                scale: Scale {
                    scale,
                    ..Scale::default()
                },
                ..Cell::default()
            };
            let second_cell = Cell {
                char: Character::new(grapheme, 1),
                style: Style {
                    fg: Color::Index(color_idx),
                    ..Style::default()
                },
                ..Cell::default()
            };
            child.write_cell(col * u16::from(scale), 0, scaled);
            child.write_cell(col, u16::from(scale).saturating_sub(1), second_cell.clone());
            child.write_cell(col, u16::from(scale), second_cell);
        }

        vx.render(&mut tty.writer())?;
    }

    // Unblock the reader parked in a blocking read, join it, then reset the
    // terminal (leave the alt screen, show the cursor, drop mouse/kitty modes).
    input_loop.signal_stop();
    let _ = vx.device_status_report(&mut tty.writer());
    input_loop.stop();
    vx.reset_state(&mut tty.writer())?;
    Ok(())
}
