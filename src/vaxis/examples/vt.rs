//! Port of upstream `examples/vt.zig`: embeds a `Terminal` widget running
//! `$SHELL` in a bordered child window. It pumps the emulator's events, forwards
//! key presses into the child, and redraws only when something changed. Ctrl-C
//! or the child exiting quits.

use std::error::Error;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use vaxis::Winsize;
use vaxis::event::Event as VxEvent;
use vaxis::event_loop::{FromEvent, Loop};
use vaxis::key::{Key, Modifiers};
use vaxis::tty::{PosixTty, Tty};
use vaxis::vaxis::{Options, Vaxis};
use vaxis::widgets::terminal::{Event as VtEvent, InputEvent, Options as VtOptions, Terminal};
use vaxis::window::{BorderOptions, BorderWhere, ChildOptions};

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
    vx.query_terminal(&mut tty.writer(), Duration::from_secs(1))?;

    let home = std::env::var_os("HOME").expect("no $HOME");
    let vt_opts = VtOptions {
        scrollback_size: 0,
        winsize: Winsize {
            rows: 24,
            cols: 100,
            x_pixel: 0,
            y_pixel: 0,
        },
        initial_working_directory: Some(PathBuf::from(home)),
    };
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string());
    let argv = [shell];
    let mut vt = Terminal::new(&argv, vt_opts)?;
    vt.spawn()?;

    let mut redraw = false;
    'main: loop {
        sleep(Duration::from_millis(8));

        // Drain emulator events first.
        while let Some(event) = vt.try_event() {
            redraw = true;
            match event {
                VtEvent::Exited => break 'main,
                VtEvent::Bell
                | VtEvent::Redraw
                | VtEvent::TitleChange(_)
                | VtEvent::PwdChange(_) => {}
            }
        }
        // Then forward input.
        while let Some(event) = input_loop.try_event() {
            redraw = true;
            match event {
                Event::KeyPress(key) => {
                    if key.matches(u32::from('c'), Modifiers::CTRL) {
                        break 'main;
                    }
                    vt.update(InputEvent::KeyPress(key))?;
                }
                Event::Winsize(ws) => vx.resize(&mut tty.writer(), ws)?,
            }
        }
        if !redraw {
            continue;
        }
        redraw = false;

        let win = vx.window();
        win.hide_cursor();
        win.clear();
        let child = win.child(ChildOptions {
            x_off: 4,
            y_off: 2,
            width: Some(120),
            height: Some(40),
            border: BorderOptions {
                location: BorderWhere::All,
                ..BorderOptions::default()
            },
        });

        vt.resize(Winsize {
            rows: child.height,
            cols: child.width,
            x_pixel: 0,
            y_pixel: 0,
        })?;
        vt.draw(&child);

        vx.render(&mut tty.writer())?;
    }

    input_loop.signal_stop();
    let _ = vx.device_status_report(&mut tty.writer());
    input_loop.stop();
    vx.reset_state(&mut tty.writer())?;
    Ok(())
}
