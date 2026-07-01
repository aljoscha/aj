//! Port of upstream `examples/text_input.zig`: a `TextInput` in a bordered
//! child window whose border color cycles per keypress. Enables mouse mode,
//! fires a desktop notification and suspends to spawn an editor on Ctrl-N, and
//! clears the input on Enter.

use std::error::Error;
use std::process::Command;
use std::time::Duration;

use vaxis::Winsize;
use vaxis::cell::{Color, Style};
use vaxis::event::Event as VxEvent;
use vaxis::event_loop::{FromEvent, Loop};
use vaxis::key::{Key, KittyFlags, Modifiers};
use vaxis::mouse::Mouse;
use vaxis::tty::{PosixTty, Tty};
use vaxis::vaxis::{Options, Vaxis};
use vaxis::widgets::text_input::{Event as InputEvent, TextInput};
use vaxis::window::{BorderOptions, BorderWhere, ChildOptions};

enum Event {
    KeyPress(Key),
    /// Constructed by `from_event` but ignored, so the payload reads as dead.
    #[allow(dead_code)]
    Mouse(Mouse),
    Winsize(Winsize),
    FocusIn,
    FocusOut,
    #[allow(dead_code)]
    Foo(u8),
}

impl FromEvent for Event {
    fn from_event(event: VxEvent) -> Option<Self> {
        match event {
            VxEvent::KeyPress(key) => Some(Event::KeyPress(key)),
            VxEvent::Mouse(m) => Some(Event::Mouse(m)),
            VxEvent::Winsize(ws) => Some(Event::Winsize(ws)),
            VxEvent::FocusIn => Some(Event::FocusIn),
            VxEvent::FocusOut => Some(Event::FocusOut),
            _ => None,
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut tty = PosixTty::new()?;
    // Request key-release events too, matching upstream's `report_events`.
    let mut vx = Vaxis::new(Options {
        kitty_keyboard_flags: KittyFlags::default() | KittyFlags::REPORT_EVENTS,
        ..Options::default()
    });
    let mut input_loop = Loop::<Event>::init(&tty, &vx)?;
    input_loop.install_resize_handler(&tty)?;
    input_loop.start();

    vx.enter_alt_screen(&mut tty.writer())?;

    let mut color_idx: u8 = 0;
    let mut text_input = TextInput::new();

    vx.set_mouse_mode(&mut tty.writer(), true)?;
    vx.query_terminal(&mut tty.writer(), Duration::from_secs(1))?;

    'main: loop {
        match input_loop.next_event() {
            Event::KeyPress(key) => {
                color_idx = color_idx.wrapping_add(1);
                if key.matches(u32::from('c'), Modifiers::CTRL) {
                    break 'main;
                } else if key.matches(u32::from('l'), Modifiers::CTRL) {
                    vx.queue_refresh();
                } else if key.matches(u32::from('n'), Modifiers::CTRL) {
                    vx.notify(&mut tty.writer(), Some("vaxis"), "hello from vaxis")?;
                    // Suspend: stop the reader, run the editor with the tty
                    // inherited, then resume and force a full repaint.
                    input_loop.signal_stop();
                    let _ = vx.device_status_report(&mut tty.writer());
                    input_loop.stop();
                    let _ = Command::new("nvim").status();
                    input_loop.start();
                    vx.enter_alt_screen(&mut tty.writer())?;
                    vx.queue_refresh();
                } else if key.matches(Key::ENTER, Modifiers::empty())
                    || key.matches(u32::from('j'), Modifiers::CTRL)
                {
                    text_input.clear_and_free();
                } else {
                    text_input.update(&InputEvent::KeyPress(key));
                }
            }
            Event::Winsize(ws) => vx.resize(&mut tty.writer(), ws)?,
            _ => {}
        }

        let win = vx.window();
        win.clear();

        let style = Style {
            fg: Color::Index(color_idx),
            ..Style::default()
        };
        let child = win.child(ChildOptions {
            x_off: i32::from(win.width / 2) - 20,
            y_off: i32::from(win.height / 2) - 3,
            width: Some(40),
            height: Some(3),
            border: BorderOptions {
                location: BorderWhere::All,
                style,
                ..BorderOptions::default()
            },
        });
        text_input.draw(child);

        vx.render(&mut tty.writer())?;
    }

    input_loop.signal_stop();
    let _ = vx.device_status_report(&mut tty.writer());
    input_loop.stop();
    vx.reset_state(&mut tty.writer())?;
    Ok(())
}
