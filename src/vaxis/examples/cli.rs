//! Port of upstream `examples/cli.zig`: a single-line `TextInput` on the
//! primary screen, with Tab/Shift-Tab cycling a small option picker whose
//! choice is inserted at the cursor on Enter. Deliberately stays off the alt
//! screen, like upstream.

use std::error::Error;
use std::time::Duration;

use vaxis::Winsize;
use vaxis::cell::{Segment, Style};
use vaxis::event::Event as VxEvent;
use vaxis::event_loop::{FromEvent, Loop};
use vaxis::key::{Key, Modifiers};
use vaxis::tty::{PosixTty, Tty};
use vaxis::vaxis::{Options, Vaxis};
use vaxis::widgets::text_input::{Event as InputEvent, TextInput};
use vaxis::window::PrintOptions;

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
    let mut tty = PosixTty::new()?;
    let mut vx = Vaxis::new(Options::default());
    let mut input_loop = Loop::<Event>::init(&tty, &vx)?;
    input_loop.install_resize_handler(&tty)?;
    input_loop.start();

    vx.query_terminal(&mut tty.writer(), Duration::from_secs(1))?;

    let mut text_input = TextInput::new();
    let mut selected_option: Option<usize> = None;
    let options = ["option 1", "option 2", "option 3"];

    'main: loop {
        match input_loop.next_event() {
            Event::KeyPress(key) => {
                if key.codepoint == u32::from('c') && key.mods.contains(Modifiers::CTRL) {
                    break 'main;
                } else if key.matches(Key::TAB, Modifiers::empty()) {
                    selected_option = Some(match selected_option {
                        None => 0,
                        Some(i) => (options.len() - 1).min(i + 1),
                    });
                } else if key.matches(Key::TAB, Modifiers::SHIFT) {
                    selected_option = Some(match selected_option {
                        None => 0,
                        Some(i) => i.saturating_sub(1),
                    });
                } else if key.matches(Key::ENTER, Modifiers::empty())
                    || key.matches(u32::from('j'), Modifiers::CTRL)
                {
                    if let Some(i) = selected_option {
                        text_input.insert_slice_at_cursor(options[i]);
                        selected_option = None;
                    }
                } else if selected_option.is_none() {
                    text_input.update(&InputEvent::KeyPress(key));
                }
            }
            Event::Winsize(ws) => vx.resize(&mut tty.writer(), ws)?,
            _ => {}
        }

        let win = vx.window();
        win.clear();

        text_input.draw(win);

        if let Some(i) = selected_option {
            win.hide_cursor();
            for (j, opt) in options.iter().enumerate() {
                let seg = Segment {
                    text: (*opt).to_string(),
                    style: if j == i {
                        Style {
                            reverse: true,
                            ..Style::default()
                        }
                    } else {
                        Style::default()
                    },
                    ..Segment::default()
                };
                win.print(
                    &[seg],
                    PrintOptions {
                        row_offset: u16::try_from(j + 1).unwrap_or(u16::MAX),
                        ..PrintOptions::default()
                    },
                );
            }
        }

        vx.render(&mut tty.writer())?;
    }

    input_loop.signal_stop();
    let _ = vx.device_status_report(&mut tty.writer());
    input_loop.stop();
    vx.reset_state(&mut tty.writer())?;
    Ok(())
}
