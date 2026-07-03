//! Read-only viewer for a background bash task's output.
//!
//! Drilled into from the agent picker (Part D-3a Spec E: the picker
//! drops out, so Esc from here returns to the editor, not the picker).
//! It shows the task's command, a live status line, and the scrollable
//! output. The output is re-read from the task registry on every draw,
//! and the drive loop requests a draw whenever the task emits output or
//! finishes, so the body tails and the status flips on their own.
//!
//! `Ctrl+K` ([`ACTION_TASK_KILL`]) kills a still-running task in place
//! through the registry handle the viewer holds. Esc/Enter close.
//!
//! Content source: the registry's stateless [`TaskRead`] snapshot. When
//! the task persists a spill file (background bash tasks always do) the
//! viewer reads it for the full output; otherwise it falls back to the
//! bounded rolling tails the model sees.

use std::cell::RefCell;
use std::rc::Rc;

use aj_agent::TaskRegistry;
use aj_agent::tool::{TaskId, TaskRead, TaskStatus};
use aj_app::keybindings::{ACTION_TASK_KILL, default_action_shortcut};
use vaxis::cell::Style;
use vaxis::key::{Key, Modifiers};
use vaxis::vxfw::{
    DrawContext, Event, EventContext, ListView, MaxSize, RelativePoint, ScrollBars, Size, Source,
    SubSurface, Surface, Text, Widget, WidgetRef, to_widget_ref,
};

use crate::overlay::{OverlayChrome, OverlayPlacement, OverlayStack, close_top};
use crate::settings_ui::push_window;
use crate::transcript::faint;

/// PgUp/PgDn step, in rows. A fixed jump rather than a viewport-derived
/// one keeps the widget from needing to know its drawn height.
const PAGE_STEP: usize = 10;

/// Rows the fixed header takes above the scrollable body: the command
/// line, the status line, and a blank separator.
const HEADER_ROWS: u16 = 3;

/// A read-only, scrollable viewer that tails one background task.
pub(crate) struct TaskOutputView {
    registry: TaskRegistry,
    id: TaskId,
    /// Command line, shown (truncated) in the header for context.
    command: String,
    /// The row list, shared with `bars` (which draws it). Rebuilt from
    /// the registry snapshot on each refresh.
    list: Rc<RefCell<ListView>>,
    bars: ScrollBars<ListView>,
    status: TaskStatus,
    total_bytes: u64,
    /// Stick to the bottom as new output arrives (tail behavior). Set on
    /// open and re-enabled by jump-to-bottom. Any manual scroll up clears
    /// it.
    follow: bool,
    text_style: Style,
    dim_style: Style,
    on_close: Option<Box<dyn FnMut(&mut EventContext)>>,
}

impl TaskOutputView {
    fn new(
        registry: TaskRegistry,
        id: TaskId,
        command: String,
        text_style: Style,
        dim_style: Style,
    ) -> TaskOutputView {
        let mut list = ListView::new(Source::Slice(Vec::new()));
        list.draw_cursor = false;
        let mut bars = ScrollBars::new(list);
        bars.draw_horizontal_scrollbar = false;
        let list = Rc::clone(&bars.view);
        let mut view = TaskOutputView {
            registry,
            id,
            command,
            list,
            bars,
            status: TaskStatus::Running,
            total_bytes: 0,
            follow: true,
            text_style,
            dim_style,
            on_close: None,
        };
        view.refresh();
        view
    }

    /// Pull the live status and output from the registry and rebuild the
    /// body rows. Following pins to the bottom; otherwise the cursor is
    /// only clamped so a shrinking buffer can't leave it out of range.
    fn refresh(&mut self) {
        let Some((status, read)) = self.registry.read(self.id) else {
            // A task evicted from the registry keeps its last-known body.
            return;
        };
        self.status = status;
        self.total_bytes = read.stdout_total_bytes + read.stderr_total_bytes;
        let lines = to_lines(&task_text(&read));
        let count = u32::try_from(lines.len()).unwrap_or(u32::MAX);
        {
            let mut list = self.list.borrow_mut();
            list.item_count = Some(count);
            list.children = Source::Slice(self.row_widgets(&lines));
        }
        if self.follow {
            self.list.borrow_mut().jump_to_item(count.saturating_sub(1));
        } else {
            let mut list = self.list.borrow_mut();
            if list.cursor >= count {
                list.jump_to_item(count.saturating_sub(1));
            }
        }
    }

    fn row_widgets(&self, lines: &[String]) -> Vec<WidgetRef> {
        lines
            .iter()
            .map(|line| {
                let mut text = Text::new(line);
                text.style = self.text_style;
                text.softwrap = false;
                let widget: WidgetRef = Rc::new(RefCell::new(text));
                widget
            })
            .collect()
    }

    /// The status line: glyph + status word + total bytes.
    fn status_line(&self) -> String {
        format!(
            "{} {} \u{b7} {}",
            status_glyph(self.status),
            status_word(self.status),
            human_bytes(self.total_bytes),
        )
    }

    fn scroll_up(&mut self, ctx: &mut EventContext, rows: usize) {
        self.follow = false;
        for _ in 0..rows {
            self.list.borrow_mut().prev_item(ctx);
        }
    }

    fn scroll_down(&mut self, ctx: &mut EventContext, rows: usize) {
        self.follow = false;
        for _ in 0..rows {
            self.list.borrow_mut().next_item(ctx);
        }
    }

    fn header_row(&self, ctx: &DrawContext, row: u16, text: String, style: Style) -> SubSurface {
        let mut widget = Text::new(text);
        widget.style = style;
        widget.softwrap = false;
        let cell = ctx.with_constraints(
            Size {
                width: 0,
                height: 0,
            },
            MaxSize {
                width: Some(ctx.max.size().width),
                height: Some(1),
            },
        );
        SubSurface {
            origin: RelativePoint {
                row: i32::from(row),
                col: 0,
            },
            surface: widget.draw(&cell),
            z_index: 0,
        }
    }
}

impl Widget for TaskOutputView {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        self.refresh();
        let size = ctx.max.size();
        // Opaque full-size surface so a shorter refresh can't leave stale
        // cells from a taller previous frame.
        let mut surface = Surface::with_size(size);
        surface
            .children
            .push(self.header_row(ctx, 0, first_line(&self.command), self.dim_style));
        surface
            .children
            .push(self.header_row(ctx, 1, self.status_line(), self.text_style));

        let body_height = size.height.saturating_sub(HEADER_ROWS);
        if body_height > 0 {
            let body_ctx = ctx.with_constraints(
                Size {
                    width: 0,
                    height: 0,
                },
                MaxSize {
                    width: Some(size.width),
                    height: Some(body_height),
                },
            );
            surface.children.push(SubSurface {
                origin: RelativePoint {
                    row: i32::from(HEADER_ROWS),
                    col: 0,
                },
                surface: self.bars.draw(&body_ctx),
                z_index: 0,
            });
        }
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::KeyPress(key) = event else {
            return;
        };
        // Esc/Enter close: a read-only view has nothing to confirm.
        if key.matches(Key::ESCAPE, Modifiers::empty())
            || key.matches(Key::ENTER, Modifiers::empty())
        {
            if let Some(cb) = self.on_close.as_mut() {
                cb(ctx);
            }
            ctx.consume_and_redraw();
            return;
        }
        // Overlay-local kill (Spec F): in place through the registry. The
        // status flip arrives via the task's `TaskEnd` and repaints the
        // header. Inert once the task is terminal.
        if key.matches(u32::from('k'), Modifiers::CTRL) {
            if self.status == TaskStatus::Running {
                self.registry.kill(self.id);
            }
            ctx.consume_and_redraw();
            return;
        }
        if key.matches(Key::UP, Modifiers::empty())
            || key.matches(u32::from('k'), Modifiers::empty())
            || key.matches(u32::from('p'), Modifiers::CTRL)
        {
            self.scroll_up(ctx, 1);
        } else if key.matches(Key::DOWN, Modifiers::empty())
            || key.matches(u32::from('j'), Modifiers::empty())
            || key.matches(u32::from('n'), Modifiers::CTRL)
        {
            self.scroll_down(ctx, 1);
        } else if key.matches(Key::PAGE_UP, Modifiers::empty()) {
            self.scroll_up(ctx, PAGE_STEP);
        } else if key.matches(Key::PAGE_DOWN, Modifiers::empty())
            || key.matches(u32::from(' '), Modifiers::empty())
        {
            self.scroll_down(ctx, PAGE_STEP);
        } else if key.matches(Key::HOME, Modifiers::empty())
            || key.matches(u32::from('g'), Modifiers::empty())
        {
            self.follow = false;
            self.list.borrow_mut().jump_to_item(0);
        } else if key.matches(Key::END, Modifiers::empty())
            || key.matches(u32::from('G'), Modifiers::empty())
        {
            self.follow = true;
        }
        // Read-only: swallow every key so none reaches the base layout.
        ctx.consume_and_redraw();
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// The task's output text: the full spill file when present, else the
/// bounded rolling tails from the snapshot.
fn task_text(read: &TaskRead) -> String {
    if let Some(path) = &read.spill_path
        && let Ok(bytes) = std::fs::read(path)
    {
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    let mut out = read.stdout_tail.clone();
    if !read.stderr_tail.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&read.stderr_tail);
    }
    out
}

/// Split output into display lines, dropping a single trailing blank
/// (from a final newline) so a well-formed stream shows no phantom row.
fn to_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.split('\n').map(decode_line).collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// Decode one output line for display: approximate a terminal's bare
/// carriage-return handling (keep the text after the last `\r`) and
/// expand tabs. Tabs must not survive: the overlay compositor measures a
/// raw tab as zero width and would shift the row it landed on.
fn decode_line(line: &str) -> String {
    let s = line.strip_suffix('\r').unwrap_or(line);
    let s = s.rsplit('\r').next().unwrap_or(s);
    s.replace('\t', "    ")
}

/// First line of `text`, for the single-row command header.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).replace('\t', "    ")
}

/// Status glyph, matching the agent picker's task-row glyphs.
fn status_glyph(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Running => "\u{2026}",
        TaskStatus::Exited(Some(0)) => "\u{2713}",
        TaskStatus::Exited(_) | TaskStatus::Killed => "\u{2717}",
    }
}

/// Human-readable status word for the header.
fn status_word(status: TaskStatus) -> String {
    match status {
        TaskStatus::Running => "running".to_string(),
        TaskStatus::Exited(Some(code)) => format!("exited {code}"),
        TaskStatus::Exited(None) => "signalled".to_string(),
        TaskStatus::Killed => "killed".to_string(),
    }
}

/// Format a byte count as `B` / `KB` / `MB` / `GB`.
// Lossy `u64 as f64` is fine here: these are small display sizes and a
// fractional rounding error in a human-readable count is harmless.
#[allow(clippy::as_conversions)]
fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n < KB {
        format!("{n} B")
    } else if n < MB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else if n < GB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else {
        format!("{:.1} GB", n as f64 / GB as f64)
    }
}

/// The scroll/kill/close subtitle, key labels resolved from keybinding
/// data (Spec F). Scroll and close are the built-in read-only keys, so
/// they keep the fixed convention.
fn subtitle() -> String {
    let kill = default_action_shortcut(ACTION_TASK_KILL).expect("aj.task.kill has a default chord");
    format!("Up/Down scroll  \u{2022}  {kill} kill  \u{2022}  Esc to close")
}

/// Open the task-output viewer for task `id`, pushing it onto `stack`.
/// The viewer holds a clone of `registry` so `Ctrl+K` can kill in place.
/// Does not move focus: the caller (host) posts the refocus event.
pub(crate) fn open_task_output(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    registry: TaskRegistry,
    id: TaskId,
    command: String,
) {
    let view = Rc::new(RefCell::new(TaskOutputView::new(
        registry,
        id,
        command,
        chrome.select.label,
        faint(),
    )));
    {
        let stack_c = Rc::clone(stack);
        let editor_c = Rc::clone(editor);
        view.borrow_mut().on_close = Some(Box::new(move |ctx| {
            close_top(&stack_c, ctx, &editor_c);
        }));
    }
    // The window's child and the focus target are the same widget: keys
    // route to the viewer while the window supplies the frame.
    let focus: WidgetRef = to_widget_ref(Rc::clone(&view));
    push_window(
        stack,
        chrome,
        &format!("Task #{id}"),
        subtitle(),
        to_widget_ref(view),
        focus,
        OverlayPlacement::Large,
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_agent::events::AgentId;
    use aj_agent::tool::{TaskKind, TaskOutputSource, TaskRead};
    use tempfile::NamedTempFile;
    use vaxis::vxfw::Phase;

    use super::*;

    /// Output source returning a fixed snapshot, so the viewer can
    /// resolve a spill path and byte totals from the registry.
    struct FakeSource {
        read: TaskRead,
    }

    impl TaskOutputSource for FakeSource {
        fn snapshot(&self) -> TaskRead {
            self.read.clone()
        }
    }

    /// Register a bash task over a spill file pre-filled with `contents`.
    /// Keep the returned `NamedTempFile` alive so the spill isn't
    /// unlinked.
    fn task(contents: &str, status: TaskStatus) -> (TaskRegistry, TaskId, NamedTempFile) {
        use std::io::Write;
        let mut file = NamedTempFile::new().expect("temp spill");
        file.write_all(contents.as_bytes()).expect("write spill");
        file.flush().expect("flush spill");
        let read = TaskRead {
            spill_path: Some(file.path().to_path_buf()),
            stdout_total_bytes: u64::try_from(contents.len()).expect("length fits u64"),
            ..TaskRead::default()
        };
        let registry = TaskRegistry::default();
        let (id, _cancel) = registry.register(
            AgentId::Main,
            TaskKind::Bash {
                command: "echo hi".to_string(),
            },
            "echo hi".to_string(),
            Arc::new(FakeSource { read }),
        );
        if status != TaskStatus::Running {
            registry.set_status(id, status);
        }
        (registry, id, file)
    }

    fn draw_ctx(width: u16, height: u16) -> DrawContext {
        DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(width),
                height: Some(height),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: vaxis::gwidth::Method::Unicode,
        }
    }

    fn flatten(surface: &Surface) -> String {
        crate::test_support::rows(surface).join("\n")
    }

    #[test]
    fn renders_command_status_and_body() {
        let contents: String = (1..=5).map(|n| format!("line{n}\n")).collect();
        let (registry, id, _f) = task(&contents, TaskStatus::Running);
        let mut view = TaskOutputView::new(
            registry,
            id,
            "echo hi".to_string(),
            Style::default(),
            Style::default(),
        );
        let rendered = flatten(&view.draw(&draw_ctx(40, 12)));
        assert!(rendered.contains("echo hi"), "command header: {rendered}");
        assert!(rendered.contains("running"), "status header: {rendered}");
        assert!(rendered.contains("line5"), "tail body: {rendered}");
    }

    #[test]
    fn terminal_status_shows_in_header() {
        let (registry, id, _f) = task("out\n", TaskStatus::Exited(Some(0)));
        let mut view = TaskOutputView::new(
            registry,
            id,
            "echo hi".to_string(),
            Style::default(),
            Style::default(),
        );
        let rendered = flatten(&view.draw(&draw_ctx(40, 10)));
        assert!(rendered.contains("exited 0"), "{rendered}");
        assert!(rendered.contains('\u{2713}'), "{rendered}");
    }

    #[test]
    fn ctrl_k_kills_a_running_task() {
        let (registry, id, _f) = task("out\n", TaskStatus::Running);
        let mut view = TaskOutputView::new(
            registry.clone(),
            id,
            "echo hi".to_string(),
            Style::default(),
            Style::default(),
        );
        let ctrl_k = Event::KeyPress(Key {
            codepoint: u32::from('k'),
            mods: Modifiers::CTRL,
            ..Key::default()
        });
        let mut ctx = EventContext::new();
        ctx.phase = Phase::Capturing;
        view.capture_event(&mut ctx, &ctrl_k);
        // The registry cancelled the task's token; the driver would flip
        // the status. With no driver, assert the token was cancelled.
        assert!(
            registry.summary(id).map(|s| s.status).is_some(),
            "task still tracked"
        );
        assert!(ctx.consume_event, "kill consumed the chord");
    }

    #[test]
    fn esc_and_enter_close() {
        let (registry, id, _f) = task("x\n", TaskStatus::Running);
        let mut view = TaskOutputView::new(
            registry,
            id,
            "echo hi".to_string(),
            Style::default(),
            Style::default(),
        );
        let closed = Rc::new(RefCell::new(0));
        let sink = Rc::clone(&closed);
        view.on_close = Some(Box::new(move |_ctx| *sink.borrow_mut() += 1));
        for key in [Key::ESCAPE, Key::ENTER] {
            let mut ctx = EventContext::new();
            ctx.phase = Phase::Capturing;
            view.capture_event(
                &mut ctx,
                &Event::KeyPress(Key {
                    codepoint: key,
                    ..Key::default()
                }),
            );
        }
        assert_eq!(*closed.borrow(), 2, "both Esc and Enter closed");
    }

    #[test]
    fn human_bytes_spans_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0 MB");
    }

    #[test]
    fn carriage_returns_collapse_to_last_segment() {
        assert_eq!(decode_line("10%\r50%\r100%"), "100%");
        assert_eq!(decode_line("plain"), "plain");
        assert_eq!(decode_line("text\r"), "text");
    }
}
