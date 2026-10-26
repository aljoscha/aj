//! Smoke tests for the embedded terminal orchestrator.
//!
//! These spawn a real child on a real PTY, so they only run on Linux and skip
//! (with a logged reason) when the sandbox has no PTY or cannot exec. They poll
//! with a timeout rather than sleeping a fixed amount, and run serially because
//! each one spawns a process and allocates a PTY.

#![cfg(target_os = "linux")]

use std::cell::RefCell;
use std::thread;
use std::time::{Duration, Instant};

use serial_test::serial;
use vaxis::Winsize;
use vaxis::key::Key;
use vaxis::screen::Screen;
use vaxis::widgets::terminal::{Event, InputEvent, Options, Terminal};
use vaxis::window::Window;

/// Overall timeout for a poll loop. Generous, since we exit early on success.
const TIMEOUT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(20);

fn winsize(cols: u16, rows: u16) -> Winsize {
    Winsize {
        rows,
        cols,
        x_pixel: 0,
        y_pixel: 0,
    }
}

/// Builds a `Terminal`, returning `None` (with a logged reason) if the PTY
/// cannot be opened so the caller skips.
fn try_new(argv: &[&str], opts: Options) -> Option<Terminal> {
    match Terminal::new(argv, opts) {
        Ok(term) => Some(term),
        Err(err) => {
            eprintln!("vaxis: skipping terminal test, init failed: {err}");
            None
        }
    }
}

/// Spawns the child, returning `false` (with a logged reason) if fork/exec
/// fails so the caller skips.
fn try_spawn(term: &mut Terminal) -> bool {
    match term.spawn() {
        Ok(()) => true,
        Err(err) => {
            eprintln!("vaxis: skipping terminal test, spawn failed: {err}");
            false
        }
    }
}

/// A screen and a full-size window over it for the terminal to draw into.
fn make_screen(cols: u16, rows: u16) -> RefCell<Screen> {
    RefCell::new(Screen::new(winsize(cols, rows)))
}

fn make_window(screen: &RefCell<Screen>, cols: u16, rows: u16) -> Window<'_> {
    Window {
        x_off: 0,
        y_off: 0,
        parent_x_off: 0,
        parent_y_off: 0,
        width: cols,
        height: rows,
        screen,
    }
}

/// Concatenates the graphemes of one window row into a string.
fn row_text(win: &Window, row: u16) -> String {
    let mut out = String::new();
    for col in 0..win.width {
        if let Some(cell) = win.read_cell(col, row) {
            out.push_str(cell.char.grapheme());
        }
    }
    out
}

#[test]
#[serial]
fn spawn_prints_hello_and_exits() {
    let Some(mut term) = try_new(&["sh", "-c", "printf hello"], Options::default()) else {
        return;
    };
    if !try_spawn(&mut term) {
        return;
    }

    let screen = make_screen(80, 24);
    let win = make_window(&screen, 80, 24);

    let mut saw_hello = false;
    let mut saw_exit = false;
    let start = Instant::now();
    while start.elapsed() < TIMEOUT {
        while let Some(ev) = term.try_event() {
            if ev == Event::Exited {
                saw_exit = true;
            }
        }
        term.draw(&win);
        if row_text(&win, 0).contains("hello") {
            saw_hello = true;
        }
        if saw_hello && saw_exit {
            break;
        }
        thread::sleep(POLL);
    }

    assert!(saw_hello, "expected 'hello' on the top row");
    assert!(saw_exit, "expected an Exited event");
}

#[test]
#[serial]
fn osc_sets_window_title() {
    // The format string carries a raw ESC and BEL, so printf emits the OSC 0
    // sequence verbatim without needing shell escape portability.
    let Some(mut term) = try_new(
        &["sh", "-c", "printf '\x1b]0;mytitle\x07'"],
        Options::default(),
    ) else {
        return;
    };
    if !try_spawn(&mut term) {
        return;
    }

    let mut title = None;
    let start = Instant::now();
    while start.elapsed() < TIMEOUT {
        while let Some(ev) = term.try_event() {
            if let Event::TitleChange(t) = ev {
                title = Some(t);
            }
        }
        if title.is_some() {
            break;
        }
        thread::sleep(POLL);
    }

    assert_eq!(title.as_deref(), Some("mytitle"));
}

#[test]
#[serial]
fn resize_updates_child_reported_size() {
    // The child cannot inspect the size until terminal input releases its read.
    let Some(mut term) = try_new(
        &[
            "sh",
            "-c",
            "stty -echo; printf 'waiting\\n'; read -r gate && printf 'released\\n' && stty size",
        ],
        Options::default(),
    ) else {
        return;
    };
    if !try_spawn(&mut term) {
        return;
    }

    let screen = make_screen(100, 40);
    let win = make_window(&screen, 100, 40);

    let start = Instant::now();
    loop {
        while term.try_event().is_some() {}
        term.draw(&win);
        if row_text(&win, 0).trim_end() == "waiting" {
            break;
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "child did not reach the read gate"
        );
        thread::sleep(POLL);
    }
    assert!(
        row_text(&win, 1).trim().is_empty(),
        "child is held at the gate"
    );
    term.resize(winsize(100, 40)).expect("resize");
    term.update(InputEvent::KeyPress(Key {
        codepoint: Key::ENTER,
        ..Key::default()
    }))
    .expect("release child through terminal input");

    let mut saw_release = false;
    let mut saw_size = false;
    let start = Instant::now();
    while start.elapsed() < TIMEOUT {
        while term.try_event().is_some() {}
        term.draw(&win);
        for row in 0..win.height {
            let text = row_text(&win, row);
            saw_release |= text.trim_end() == "released";
            saw_size |= text.trim_end() == "40 100";
        }
        if saw_release && saw_size {
            break;
        }
        thread::sleep(POLL);
    }

    assert!(saw_release, "child did not acknowledge the gate release");
    assert!(
        saw_size,
        "expected the child to report the resized 40x100 size"
    );
}
