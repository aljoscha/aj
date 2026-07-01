//! Port of upstream `examples/vaxis.zig`: an animated branding screen. Queries
//! the terminal's fg/bg colors, then pulses the centered logo by blending the
//! background toward the foreground each frame. Ctrl-C quits.
//!
//! Upstream sets `pub const panic = vaxis.panic_handler`. Our equivalent is
//! automatic: `PosixTty::new` installs a panic hook that resets the terminal.

use std::error::Error;
use std::thread::sleep;
use std::time::Duration;

use vaxis::Winsize;
use vaxis::cell::{Color, Kind, Segment, Style};
use vaxis::event::Event as VxEvent;
use vaxis::event_loop::{FromEvent, Loop};
use vaxis::key::{Key, Modifiers};
use vaxis::tty::{PosixTty, Tty};
use vaxis::vaxis::{Options, Vaxis};
use vaxis::widgets::alignment;
use vaxis::window::{PrintOptions, Wrap};

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

/// Blends toward `b` from `a` by `pct` percent, per channel, saturating at 255.
fn blend_colors(a: [u8; 3], b: [u8; 3], pct: u8) -> Color {
    let mix = |ca: u8, cb: u8| -> u8 {
        let from = u16::from(ca) * u16::from(100u8.saturating_sub(pct)) / 100;
        let to = u16::from(cb) * u16::from(pct) / 100;
        u8::try_from((from + to).min(255)).unwrap_or(255)
    };
    Color::Rgb([mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2])])
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut tty = PosixTty::new()?;
    let mut vx = Vaxis::new(Options::default());
    let mut input_loop = Loop::<Event>::init(&tty, &vx)?;
    input_loop.install_resize_handler(&tty)?;
    input_loop.start();

    vx.enter_alt_screen(&mut tty.writer())?;
    vx.query_terminal(&mut tty.writer(), Duration::from_secs(1))?;

    vx.query_color(&mut tty.writer(), Kind::Fg)?;
    vx.query_color(&mut tty.writer(), Kind::Bg)?;

    let mut pct: u8 = 0;
    let mut going_up = true;

    let fg = [192u8, 202, 245];
    let bg = [26u8, 27, 38];

    'main: {
        // Block until we get the first resize, so we know the screen size.
        loop {
            match input_loop.next_event() {
                Event::KeyPress(key) if key.matches(u32::from('c'), Modifiers::CTRL) => {
                    break 'main;
                }
                Event::Winsize(ws) => {
                    vx.resize(&mut tty.writer(), ws)?;
                    break;
                }
                Event::KeyPress(_) => {}
            }
        }

        loop {
            while let Some(event) = input_loop.try_event() {
                match event {
                    Event::KeyPress(key) if key.matches(u32::from('c'), Modifiers::CTRL) => {
                        break 'main;
                    }
                    Event::Winsize(ws) => vx.resize(&mut tty.writer(), ws)?,
                    Event::KeyPress(_) => {}
                }
            }

            let win = vx.window();
            win.clear();

            let color = blend_colors(bg, fg, pct);
            let segment = Segment {
                text: vaxis::LOGO.to_string(),
                style: Style {
                    fg: color,
                    ..Style::default()
                },
                ..Segment::default()
            };
            let center = alignment::center(win, 28, 4);
            center.print_segment(
                segment,
                PrintOptions {
                    wrap: Wrap::Grapheme,
                    ..PrintOptions::default()
                },
            );
            vx.render(&mut tty.writer())?;

            sleep(Duration::from_millis(16));
            if going_up {
                pct += 1;
                if pct == 100 {
                    going_up = false;
                }
            } else {
                pct -= 1;
                if pct == 0 {
                    going_up = true;
                }
            }
        }
    }

    input_loop.signal_stop();
    let _ = vx.device_status_report(&mut tty.writer());
    input_loop.stop();
    vx.reset_state(&mut tty.writer())?;
    Ok(())
}
