//! Read-only viewer for a background bash task's output.
//!
//! Drilled into from the agent picker, which drops out on the way in, so
//! Esc from here returns to the editor, not the picker.
//! It shows the task's command, a live status line, and the scrollable
//! output. The body follows new output until the user scrolls away. End resumes
//! following. Status and output update through the same read in every mode.
//!
//! `Ctrl+K` ([`ACTION_TASK_KILL`]) parks a still-running task's id for the
//! drive loop to kill through the session command gate. Esc/Enter close.
//!
//! Output arrives in bounded byte chunks through `Control`. Reads are polled
//! alongside terminal input, never from drawing. Closing drops the pending read.

use std::cell::RefCell;
use std::rc::Rc;

use aj_agent::tool::{TaskId, TaskStatus};
use aj_app::keybindings::{ACTION_TASK_KILL, action_shortcut, format_keybinding};
use aj_wire::TaskOutput;
use futures::{FutureExt, future::LocalBoxFuture};
use std::task::{Context, Poll};
use std::time::Duration;

use crate::control::{Control, ControlError};
use vaxis::cell::Style;
use vaxis::key::{Key, Modifiers};
use vaxis::vxfw::{
    Builder, DrawContext, Event, EventContext, ListView, MaxSize, RelativePoint, ScrollBars, Size,
    Source, SubSurface, Surface, Text, Widget, WidgetRef, to_widget_ref,
};

use crate::keymap::action_matches;
use crate::overlay::{OverlayChrome, OverlayPlacement, OverlayStack, close_key_label, close_top};
use crate::settings_ui::push_window;
use crate::transcript::faint;

/// Rows the fixed header takes above the scrollable body: the command
/// line, the status line, and a blank separator.
const HEADER_ROWS: u16 = 3;

const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// A single in-flight read. Its future owns only the addressed host and session,
/// so a focus change cannot redirect an answer to another task.
struct OutputReader {
    control: Control,
    session: String,
    read: LocalBoxFuture<'static, Result<TaskOutput, ControlError>>,
    retry_delay: Duration,
}

impl OutputReader {
    fn request(&mut self, task: TaskId, offset: u64, delay: Duration) {
        let control = self.control.clone();
        let session = self.session.clone();
        self.read = async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            control.task_output(&session, task, offset).await
        }
        .boxed_local();
    }
}

/// Raw bytes keep UTF-8 and partial lines intact across chunk boundaries. Only
/// visible rows are decoded and turned into widgets.
#[derive(Default)]
struct OutputBuffer {
    bytes: Vec<u8>,
    starts: Vec<usize>,
}

impl OutputBuffer {
    fn append(&mut self, bytes: &[u8]) {
        if self.starts.is_empty() {
            self.starts.push(0);
        }
        let offset = self.bytes.len();
        self.starts.extend(
            bytes
                .iter()
                .enumerate()
                .filter_map(|(i, b)| (*b == b'\n').then_some(offset + i + 1)),
        );
        self.bytes.extend_from_slice(bytes);
    }

    fn len(&self) -> usize {
        self.starts.len() - usize::from(self.starts.last() == Some(&self.bytes.len()))
    }
}

struct OutputRows {
    buffer: Rc<RefCell<OutputBuffer>>,
    style: Style,
}

impl Builder for OutputRows {
    fn item_at_idx(&self, idx: usize, _cursor: usize) -> Option<WidgetRef> {
        let buffer = self.buffer.borrow();
        if idx >= buffer.len() {
            return None;
        }
        let start = buffer.starts[idx];
        let end = buffer
            .starts
            .get(idx + 1)
            .map_or(buffer.bytes.len(), |end| end - 1);
        let line = String::from_utf8_lossy(&buffer.bytes[start..end]);
        let mut text = Text::new(decode_line(&line));
        text.style = self.style;
        text.softwrap = false;
        Some(Rc::new(RefCell::new(text)))
    }
}

/// A read-only, scrollable viewer that tails one background task.
pub(crate) struct TaskOutputView {
    kill: Rc<RefCell<Option<TaskId>>>,
    buffer: Rc<RefCell<OutputBuffer>>,
    reader: Option<OutputReader>,
    notice: Option<String>,
    id: TaskId,
    /// Command line, shown (truncated) in the header for context.
    command: String,
    /// The row list, shared with `bars` (which draws it). Rows are built lazily
    /// from the accumulated output.
    list: Rc<RefCell<ListView>>,
    bars: Rc<RefCell<ScrollBars<ListView>>>,
    status: TaskStatus,
    total_bytes: u64,
    /// Stick to the bottom as new output arrives (tail behavior). Set on
    /// open and re-enabled by jump-to-bottom. Manual keyboard scrolling
    /// clears it.
    follow: bool,
    text_style: Style,
    dim_style: Style,
    /// Scrollbar thumb color, the shared `Muted` token so the thumb matches
    /// every other scrollbar. Distinct from `dim_style` (the header's faint
    /// attribute), which reads dim but is not the `Muted` color.
    thumb_style: Style,
    on_close: Option<Box<dyn FnMut(&mut EventContext)>>,
}

impl TaskOutputView {
    fn new(
        kill: Rc<RefCell<Option<TaskId>>>,
        id: TaskId,
        command: String,
        text_style: Style,
        dim_style: Style,
        thumb_style: Style,
    ) -> TaskOutputView {
        let buffer = Rc::new(RefCell::new(OutputBuffer::default()));
        let mut list = ListView::new(Source::Builder(Box::new(OutputRows {
            buffer: Rc::clone(&buffer),
            style: text_style,
        })));
        list.draw_cursor = false;
        let bars = ScrollBars::new(list);
        bars.borrow_mut().draw_horizontal_scrollbar = false;
        let list = Rc::clone(&bars.borrow().view);
        TaskOutputView {
            kill,
            buffer,
            reader: None,
            notice: Some("Loading output…".to_string()),
            id,
            command,
            list,
            bars,
            status: TaskStatus::Running,
            total_bytes: 0,
            follow: true,
            text_style,
            dim_style,
            thumb_style,
            on_close: None,
        }
    }

    pub(crate) fn read_from(&mut self, control: Control, session: String) {
        let mut reader = OutputReader {
            control,
            session,
            read: futures::future::pending().boxed_local(),
            retry_delay: POLL_INTERVAL,
        };
        reader.request(self.id, self.offset(), Duration::ZERO);
        self.reader = Some(reader);
    }

    fn offset(&self) -> u64 {
        u64::try_from(self.buffer.borrow().bytes.len()).expect("output length fits u64")
    }

    /// Poll once from the drive loop's select, releasing the widget borrow
    /// before waiting. A closed or fully read terminal task has no more work.
    pub(crate) fn poll_output(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let Some(reader) = self.reader.as_mut() else {
            return Poll::Pending;
        };
        let Poll::Ready(result) = reader.read.poll_unpin(cx) else {
            return Poll::Pending;
        };
        let mut reader = self.reader.take().expect("polled reader");
        match result {
            Ok(output) => {
                if output.id != self.id
                    || output.offset != self.offset()
                    || output
                        .offset
                        .checked_add(u64::try_from(output.bytes.len()).unwrap())
                        .is_none_or(|end| end > output.total_bytes)
                    || (output.bytes.is_empty() && output.offset < output.total_bytes)
                {
                    self.notice = Some("Host returned inconsistent task output.".to_string());
                    return Poll::Ready(());
                }
                let total = output.total_bytes;
                let running = output.status == TaskStatus::Running;
                self.apply_output(output);
                let caught_up = self.offset() == total;
                if running || !caught_up {
                    reader.retry_delay = POLL_INTERVAL;
                    reader.request(
                        self.id,
                        self.offset(),
                        if caught_up {
                            POLL_INTERVAL
                        } else {
                            Duration::ZERO
                        },
                    );
                    self.reader = Some(reader);
                }
            }
            Err(err) => {
                if err.unknown_endpoint() {
                    self.notice = Some(
                        "Host does not support full task output (upgrade the host).".to_string(),
                    );
                } else {
                    self.notice = Some(format!("Output unavailable: {err}"));
                    reader.request(self.id, self.offset(), reader.retry_delay);
                    reader.retry_delay = (reader.retry_delay * 2).min(Duration::from_secs(30));
                    self.reader = Some(reader);
                }
            }
        }
        Poll::Ready(())
    }

    fn apply_output(&mut self, output: TaskOutput) {
        self.status = output.status;
        self.total_bytes = output.total_bytes;
        self.buffer.borrow_mut().append(&output.bytes);
        self.notice = (self.offset() < self.total_bytes).then(|| {
            format!(
                "Loading output: {} / {}",
                human_bytes(self.offset()),
                human_bytes(self.total_bytes)
            )
        });
        let count = u32::try_from(self.buffer.borrow().len()).unwrap_or(u32::MAX);
        self.list.borrow_mut().item_count = Some(count);
        if self.follow {
            self.list.borrow_mut().scroll_to_bottom();
        }
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

    fn scroll_lines(&mut self, rows: i32) {
        self.follow = false;
        self.list.borrow_mut().scroll_lines(rows);
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

        if let Some(notice) = &self.notice {
            surface
                .children
                .push(self.header_row(ctx, 2, notice.clone(), self.dim_style));
        }
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
            // NOTE: tint the thumb from the shared Muted color token per-draw,
            // via the shared helper, so it matches every other scrollbar. This
            // is the `Muted` color, not the header's faint `dim_style`.
            let bars_surface = {
                let mut bars = self.bars.borrow_mut();
                crate::scroll::apply_thumb_style(&mut bars, self.thumb_style);
                bars.draw(&body_ctx)
            };
            surface.children.push(SubSurface {
                origin: RelativePoint {
                    row: i32::from(HEADER_ROWS),
                    col: 0,
                },
                surface: bars_surface,
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
            self.reader = None;
            if let Some(cb) = self.on_close.as_mut() {
                cb(ctx);
            }
            ctx.consume_and_redraw();
            return;
        }
        // Park kills for the drive loop's mutation gate in every mode. The
        // output read carries the resulting status. Inert once terminal.
        if action_matches(key, ACTION_TASK_KILL) {
            if self.status == TaskStatus::Running {
                *self.kill.borrow_mut() = Some(self.id);
            }
            ctx.consume_and_redraw();
            return;
        }
        if key.matches(Key::UP, Modifiers::empty())
            || key.matches(u32::from('k'), Modifiers::empty())
            || key.matches(u32::from('p'), Modifiers::CTRL)
        {
            self.scroll_lines(-1);
        } else if key.matches(Key::DOWN, Modifiers::empty())
            || key.matches(u32::from('j'), Modifiers::empty())
            || key.matches(u32::from('n'), Modifiers::CTRL)
        {
            self.scroll_lines(1);
        } else if key.matches(Key::PAGE_UP, Modifiers::empty()) {
            let page = crate::scroll::page_scroll_lines(self.list.borrow().viewport_height());
            self.scroll_lines(-page);
        } else if key.matches(Key::PAGE_DOWN, Modifiers::empty())
            || key.matches(u32::from(' '), Modifiers::empty())
        {
            let page = crate::scroll::page_scroll_lines(self.list.borrow().viewport_height());
            self.scroll_lines(page);
        } else if key.matches(Key::HOME, Modifiers::empty())
            || key.matches(u32::from('g'), Modifiers::empty())
        {
            self.follow = false;
            self.list.borrow_mut().jump_to_item(0);
        } else if key.matches(Key::END, Modifiers::empty())
            || key.matches(u32::from('G'), Modifiers::empty())
        {
            self.follow = true;
            self.list.borrow_mut().scroll_to_bottom();
        }
        // Read-only: swallow every key so none reaches the base layout.
        ctx.consume_and_redraw();
    }

    fn wants_events(&self) -> bool {
        true
    }
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
        TaskStatus::Exited(_) | TaskStatus::CaptureFailed(_) | TaskStatus::Killed => "\u{2717}",
    }
}

/// Human-readable status word for the header.
fn status_word(status: TaskStatus) -> String {
    match status {
        TaskStatus::Running => "running".to_string(),
        TaskStatus::Exited(Some(code)) => format!("exited {code}"),
        TaskStatus::Exited(None) => "signalled".to_string(),
        TaskStatus::CaptureFailed(Some(code)) => format!("capture failed, exited {code}"),
        TaskStatus::CaptureFailed(None) => "capture failed, signalled".to_string(),
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
/// data so a rebind relabels them. Scroll and close are the built-in
/// read-only keys, so they keep the fixed convention.
fn subtitle() -> String {
    let kill = action_shortcut(ACTION_TASK_KILL).expect("aj.task.kill has a default chord");
    let up = format_keybinding("up");
    let down = format_keybinding("down");
    let close = close_key_label();
    format!("{up}/{down} scroll  \u{2022}  {kill} kill  \u{2022}  {close} to close")
}

/// Push a viewer for task `id` onto `stack`. The caller binds its output source,
/// polls reads alongside input, and posts the refocus event.
pub(crate) fn open_task_output(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    kill: Rc<RefCell<Option<TaskId>>>,
    id: TaskId,
    command: String,
) -> Rc<RefCell<TaskOutputView>> {
    let view = Rc::new(RefCell::new(TaskOutputView::new(
        kill,
        id,
        command,
        chrome.select.label,
        faint(),
        chrome.select.scrollbar_thumb,
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
        to_widget_ref(Rc::clone(&view)),
        focus,
        OverlayPlacement::Large,
    );
    view
}

#[cfg(test)]
mod tests {
    use vaxis::vxfw::Phase;

    use super::*;

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

    fn press(view: &mut TaskOutputView, codepoint: u32, mods: Modifiers) {
        let mut ctx = EventContext::new();
        ctx.phase = Phase::Capturing;
        view.capture_event(
            &mut ctx,
            &Event::KeyPress(Key {
                codepoint,
                mods,
                ..Key::default()
            }),
        );
        assert!(ctx.consume_event);
    }

    fn viewer() -> TaskOutputView {
        TaskOutputView::new(
            Rc::new(RefCell::new(None)),
            7,
            "echo hi".to_string(),
            Style::default(),
            Style::default(),
            Style::default(),
        )
    }

    fn output_through(view: &mut TaskOutputView, last: u32) {
        let text: String = (1..=last).map(|n| format!("line{n}\n")).collect();
        let offset = view.offset();
        view.apply_output(TaskOutput {
            id: view.id,
            status: TaskStatus::Running,
            offset,
            total_bytes: u64::try_from(text.len()).unwrap(),
            bytes: text.as_bytes()[usize::try_from(offset).unwrap()..].to_vec(),
        });
    }

    fn assert_body(
        view: &mut TaskOutputView,
        height: u16,
        expected: impl IntoIterator<Item = u32>,
    ) {
        let surface = view.draw(&draw_ctx(40, height + HEADER_ROWS));
        // Read only the text columns, excluding the scrollbar at the right edge.
        let rows: Vec<String> = crate::test_support::flatten(&surface)
            .iter()
            .skip(usize::from(HEADER_ROWS))
            .map(|row| {
                row.iter()
                    .take(39)
                    .map(|cell| cell.char.grapheme())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .filter(|row| !row.is_empty())
            .collect();
        let expected: Vec<String> = expected.into_iter().map(|n| format!("line{n}")).collect();
        assert_eq!(rows, expected);
    }

    #[test]
    fn keyboard_scrolls_drawn_rows_by_lines_and_viewport_pages() {
        let mut view = viewer();
        output_through(&mut view, 100);
        // Reuse the view to cover paging after a resize as well as tiny bodies.
        for height in [5, 16, 2, 1] {
            press(&mut view, Key::HOME, Modifiers::empty());
            assert_body(&mut view, height, 1..=u32::from(height));
            let page = u32::try_from(crate::scroll::page_scroll_lines(Some(height))).unwrap();
            for key in [Key::PAGE_DOWN, u32::from(' ')] {
                press(&mut view, key, Modifiers::empty());
                assert_body(&mut view, height, 1 + page..=u32::from(height) + page);
                press(&mut view, Key::PAGE_UP, Modifiers::empty());
                assert_body(&mut view, height, 1..=u32::from(height));
            }
            for (down, up, mods) in [
                (Key::DOWN, Key::UP, Modifiers::empty()),
                (u32::from('j'), u32::from('k'), Modifiers::empty()),
                (u32::from('n'), u32::from('p'), Modifiers::CTRL),
            ] {
                press(&mut view, down, mods);
                assert_body(&mut view, height, 2..=u32::from(height) + 1);
                press(&mut view, up, mods);
                assert_body(&mut view, height, 1..=u32::from(height));
            }
        }
    }

    #[test]
    fn appended_output_follows_until_keyboard_navigation_and_end_resumes() {
        for (key, mods, first) in [
            (Key::UP, Modifiers::empty(), 15),
            (Key::DOWN, Modifiers::empty(), 16),
            (Key::PAGE_UP, Modifiers::empty(), 13),
            (Key::PAGE_DOWN, Modifiers::empty(), 16),
            (Key::HOME, Modifiers::empty(), 1),
            (u32::from('g'), Modifiers::empty(), 1),
        ] {
            let mut view = viewer();
            output_through(&mut view, 10);
            assert_body(&mut view, 5, 6..=10);
            output_through(&mut view, 20);
            assert_body(&mut view, 5, 16..=20);
            press(&mut view, key, mods);
            assert_body(&mut view, 5, first..=first + 4);
            output_through(&mut view, 30);
            assert_body(&mut view, 5, first..=first + 4);
            for end in [Key::END, u32::from('G')] {
                press(&mut view, Key::HOME, Modifiers::empty());
                assert_body(&mut view, 5, 1..=5);
                press(&mut view, end, Modifiers::empty());
                // End must reveal existing output without waiting for a snapshot.
                assert_body(&mut view, 5, 26..=30);
            }
            output_through(&mut view, 40);
            assert_body(&mut view, 5, 36..=40);
        }
    }

    #[test]
    fn renders_chunks_and_status_and_parks_kill_only_while_running() {
        let mut view = viewer();
        let bytes = "out\nerr\n".as_bytes().to_vec();
        view.apply_output(TaskOutput {
            id: 7,
            status: TaskStatus::Running,
            offset: 0,
            total_bytes: 8,
            bytes,
        });
        let rendered = flatten(&view.draw(&draw_ctx(40, 12)));
        for text in ["echo hi", "out", "err", "running", "8 B"] {
            assert!(rendered.contains(text), "{rendered}");
        }
        press(&mut view, u32::from('k'), Modifiers::CTRL);
        assert_eq!(*view.kill.borrow_mut(), Some(7));
        *view.kill.borrow_mut() = None;
        view.apply_output(TaskOutput {
            id: 7,
            status: TaskStatus::Exited(Some(0)),
            offset: 8,
            total_bytes: 8,
            bytes: Vec::new(),
        });
        press(&mut view, u32::from('k'), Modifiers::CTRL);
        assert_eq!(*view.kill.borrow_mut(), None);
        assert!(flatten(&view.draw(&draw_ctx(40, 12))).contains("exited 0"));
    }

    #[test]
    fn split_utf8_partial_lines_and_invalid_bytes_render_without_losing_output() {
        let mut view = viewer();
        let bytes = "first\n雪\ttab\rfinal 雪\nlast".as_bytes();
        for (i, byte) in bytes.iter().enumerate() {
            view.apply_output(TaskOutput {
                id: 7,
                status: TaskStatus::Running,
                offset: u64::try_from(i).unwrap(),
                total_bytes: u64::try_from(bytes.len()).unwrap(),
                bytes: vec![*byte],
            });
        }
        let rendered = flatten(&view.draw(&draw_ctx(40, 12)));
        for text in ["first", "final 雪", "last"] {
            assert!(rendered.contains(text), "{rendered}");
        }
        assert!(
            !rendered.contains('�'),
            "split UTF-8 was corrupted: {rendered}"
        );
        view.apply_output(TaskOutput {
            id: 7,
            status: TaskStatus::Exited(Some(0)),
            offset: u64::try_from(bytes.len()).unwrap(),
            total_bytes: u64::try_from(bytes.len() + 2).unwrap(),
            bytes: vec![0xff, b'\n'],
        });
        assert!(flatten(&view.draw(&draw_ctx(40, 12))).contains("last�"));
    }

    #[test]
    fn esc_and_enter_close() {
        let mut view = viewer();
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

    /// The scroll/kill/close subtitle resolves every key label from the
    /// keybinding data, so a rebind moves both the rendered hint and the
    /// assertion together rather than tracking a literal.
    #[test]
    fn subtitle_resolves_scroll_kill_and_close_labels() {
        let s = subtitle();
        assert!(s.contains(&format_keybinding("up")), "{s}");
        assert!(s.contains(&format_keybinding("down")), "{s}");
        assert!(
            s.contains(&action_shortcut(ACTION_TASK_KILL).unwrap()),
            "{s}"
        );
        assert!(s.contains(&close_key_label()), "{s}");
    }

    /// The tailing body's scrollbar thumb is tinted with the view's
    /// `thumb_style` (the shared `Muted` color), via the shared helper, whenever
    /// the output overflows its slot. Dropping the per-draw `apply_thumb_style`
    /// in `draw` leaves the thumb at the default fg and fails here.
    #[test]
    fn scrollbar_thumb_carries_the_muted_tint() {
        // A distinct thumb fg so the tinted thumb can't be confused with a
        // default-styled cell or the header's faint dim_style.
        let thumb = Style {
            fg: vaxis::cell::Color::Index(1),
            ..Style::default()
        };
        let mut view = TaskOutputView::new(
            Rc::new(RefCell::new(None)),
            7,
            "echo hi".to_string(),
            Style::default(),
            Style::default(),
            thumb,
        );
        output_through(&mut view, 40);
        let surface = view.draw(&draw_ctx(20, 8));
        // The thumb sits on the body's right edge, in a child surface, so
        // composite the tree before reading the cell's style.
        let last_col = 19;
        let fg = crate::test_support::flatten(&surface)
            .iter()
            .find_map(|row| {
                let cell = row.get(last_col)?;
                (cell.char.grapheme() == "\u{2590}").then_some(cell.style.fg)
            })
            .expect("a thumb cell is drawn on the body's right edge");
        assert_eq!(
            fg, thumb.fg,
            "the task-output body thumb carries the thumb_style tint"
        );
    }
}
