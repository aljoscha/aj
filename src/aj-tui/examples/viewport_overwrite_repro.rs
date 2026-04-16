//! Reproduction harness for a TUI viewport-overwrite bug.
//!
//! Run from the repo root:
//!   cargo run -p aj-tui --example viewport_overwrite_repro
//!
//! For a reliable repro, run in a small terminal (8-12 rows) — for example,
//! under tmux:
//!   tmux new-session -d -s tui-bug -x 80 -y 12
//!   tmux send-keys -t tui-bug "cargo run -p aj-tui --example viewport_overwrite_repro" Enter
//!   tmux attach -t tui-bug
//!
//! Expected behavior:
//! - PRE-TOOL lines remain visible above tool output.
//! - POST-TOOL lines append after tool output without overwriting earlier
//!   content.
//!
//! Actual behavior (bug):
//! - When content exceeds the viewport and new lines arrive after a
//!   tool-call pause, some earlier PRE-TOOL lines near the bottom are
//!   overwritten by POST-TOOL lines.
//!
//! Implementation note: the render loop here coalesces redraws through
//! the normal [`Tui::next_event`] throttle — each append calls
//! `tui.request_render()` but never `tui.render()` directly. This
//! matches the scheduling behavior of the real agent app and is
//! necessary to reproduce the bug: rendering synchronously after
//! every append changes the diff baseline and masks the issue.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use aj_tui::component::Component;
use aj_tui::impl_component_any;
use aj_tui::terminal::ProcessTerminal;
use aj_tui::tui::{Tui, TuiEvent};

/// A component that renders a growing list of lines. Cheap cloneable
/// handle — mutations through any clone are visible to the renderer.
#[derive(Clone)]
struct Lines {
    lines: Rc<RefCell<Vec<String>>>,
}

impl Lines {
    fn new() -> Self {
        Self {
            lines: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn set<I, S>(&self, iter: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        *self.lines.borrow_mut() = iter.into_iter().map(Into::into).collect();
    }

    fn append<I, S>(&self, iter: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.lines
            .borrow_mut()
            .extend(iter.into_iter().map(Into::into));
    }
}

impl Component for Lines {
    impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        self.lines
            .borrow()
            .iter()
            .map(|line| {
                if line.chars().count() > width {
                    line.chars().take(width).collect()
                } else {
                    format!("{:<width$}", line, width = width)
                }
            })
            .collect()
    }
}

/// Await `duration`, flushing renders as [`TuiEvent::Render`] fires.
/// Mirrors the scheduler behavior of the real agent app: writes
/// coalesce into at most one render per `render_interval`, which is
/// the regime in which the overwrite bug manifests.
async fn drive_for(tui: &mut Tui, duration: Duration) {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return,
            maybe = tui.next_event() => match maybe {
                Some(TuiEvent::Render) => tui.render(),
                // Ignore any input events during the repro; the harness is
                // self-driving.
                Some(TuiEvent::Input(_)) => {}
                None => return,
            }
        }
    }
}

/// Append `count` labeled lines to `buffer`, pausing `delay_ms` between
/// appends. Each append requests a render; the throttled event loop
/// coalesces them into at most one paint per window.
async fn stream_lines(buffer: &Lines, label: &str, count: usize, delay_ms: u64, tui: &mut Tui) {
    for i in 1..=count {
        buffer.append([format!("{} {:02}", label, i)]);
        tui.request_render();
        drive_for(tui, Duration::from_millis(delay_ms)).await;
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut tui = Tui::new(Box::new(ProcessTerminal::new()));
    if let Err(e) = tui.start() {
        eprintln!("Failed to start terminal: {}", e);
        return;
    }

    let height = tui.terminal().rows();
    let buffer = Lines::new();
    tui.root.add_child(Box::new(buffer.clone()));

    // Pick counts designed to exceed the viewport both before and during
    // the "tool call" so content gets pushed into scrollback.
    let pre_count = usize::from(height) + 8;
    let tool_count = usize::from(height) + 12;
    let post_count = 6usize;

    buffer.set([
        "TUI viewport overwrite repro".to_string(),
        format!("Viewport rows detected: {}", height),
        "(Resize to ~8-12 rows for best repro)".to_string(),
        String::new(),
        "=== PRE-TOOL STREAM ===".to_string(),
    ]);
    tui.request_render();
    drive_for(&mut tui, Duration::from_millis(300)).await;

    // Phase 1: stream pre-tool text until the viewport is exceeded.
    stream_lines(&buffer, "PRE-TOOL LINE", pre_count, 30, &mut tui).await;

    // Phase 2: simulate a tool-call pause, then the tool's own output.
    buffer.append([
        String::new(),
        "--- TOOL CALL START ---".to_string(),
        "(pause...)".to_string(),
        String::new(),
    ]);
    tui.request_render();
    drive_for(&mut tui, Duration::from_millis(700)).await;

    stream_lines(&buffer, "TOOL OUT", tool_count, 20, &mut tui).await;

    // Phase 3: post-tool streaming. This is where overwrite often appears.
    buffer.append([String::new(), "=== POST-TOOL STREAM ===".to_string()]);
    tui.request_render();
    drive_for(&mut tui, Duration::from_millis(300)).await;
    stream_lines(&buffer, "POST-TOOL LINE", post_count, 40, &mut tui).await;

    // Leave the final output visible briefly, then restore the terminal.
    drive_for(&mut tui, Duration::from_millis(1500)).await;
    tui.stop();
}
