//! Port of upstream `examples/text_view.zig`: a scrolling `TextView` over a
//! `Buffer`. Enter appends a numbered line, Up/Down scroll, `c` quits.

use std::error::Error;
use std::time::Duration;

use vaxis::Winsize;
use vaxis::event::Event as VxEvent;
use vaxis::event_loop::{FromEvent, Loop};
use vaxis::key::{Key, Modifiers};
use vaxis::tty::{PosixTty, Tty};
use vaxis::vaxis::{Options, Vaxis};
use vaxis::widgets::text_view::{Buffer, TextView};

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

fn main() -> Result<(), Box<dyn Error>> {
    let mut tty = PosixTty::new()?;
    let mut vx = Vaxis::new(Options::default());
    let mut input_loop = Loop::<Event>::init(&tty, &vx)?;
    input_loop.install_resize_handler(&tty)?;
    input_loop.start();

    vx.enter_alt_screen(&mut tty.writer())?;
    vx.query_terminal(&mut tty.writer(), Duration::from_secs(20))?;

    let mut text_view = TextView::default();
    let mut buffer = Buffer::default();
    buffer.append("Press Enter to add a line, Up/Down to scroll, 'c' to close.");

    let mut counter: i32 = 0;

    'main: loop {
        match input_loop.next_event() {
            Event::KeyPress(key) => {
                if key.matches(u32::from('c'), Modifiers::empty()) {
                    break 'main;
                }
                if key.matches(Key::ENTER, Modifiers::empty()) {
                    counter += 1;
                    buffer.append(&format!("\nLine {counter}"));
                }
                text_view.input(&key);
            }
            Event::Winsize(ws) => vx.resize(&mut tty.writer(), ws)?,
        }

        let win = vx.window();
        win.clear();
        text_view.draw(win, &buffer);
        vx.render(&mut tty.writer())?;
    }

    input_loop.signal_stop();
    let _ = vx.device_status_report(&mut tty.writer());
    input_loop.stop();
    vx.reset_state(&mut tty.writer())?;
    Ok(())
}
