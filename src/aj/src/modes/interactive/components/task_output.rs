//! Read-only viewer for a background bash task's output.
//!
//! Opened by pressing Enter on a background-bash-task row in the agent
//! picker. It tails the task's spill file, the canonical full output the
//! bash tool writes to disk, so the whole run is scrollable rather than
//! just the bounded rolling tail the model sees. Both Esc and Enter
//! close it, matching the other read-only overlays.
//!
//! Live updates need no extra plumbing: the viewer re-reads the registry
//! and the spill file inside `render`, and the event pump requests a
//! render whenever the task emits output or finishes, so the body tails
//! and the header status flips on its own. `ctrl+k` ([`ACTION_TASK_KILL`])
//! kills a still-running task in place through the registry handle the
//! viewer holds; the resulting status change repaints the header.
//!
//! [`ACTION_TASK_KILL`]: crate::config::keybindings::ACTION_TASK_KILL

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aj_agent::TaskRegistry;
use aj_agent::tool::{TaskId, TaskStatus};
use aj_tui::ansi::{truncate_to_width, visible_width, wrap_text_with_ansi};
use aj_tui::component::Component;
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;
use aj_tui::style;

use crate::config::keybindings::ACTION_TASK_KILL;

/// Outcome of a viewer session.
///
/// Read-only, so the only terminal state is `Closed`. Killing the task
/// is an in-place side effect (the viewer holds the registry), not a
/// close, so it produces no outcome.
#[derive(Clone, Debug)]
pub enum TaskOutputOutcome {
    Closed,
}

/// Cheap-to-clone handle pointing at the viewer's outcome slot.
#[derive(Clone)]
pub struct TaskOutputOutcomeHandle(Arc<Mutex<Option<TaskOutputOutcome>>>);

impl TaskOutputOutcomeHandle {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    /// Take the current outcome (if any), leaving the slot empty.
    pub fn take(&self) -> Option<TaskOutputOutcome> {
        self.0
            .lock()
            .expect("task output outcome mutex poisoned")
            .take()
    }

    fn set(&self, value: TaskOutputOutcome) {
        *self.0.lock().expect("task output outcome mutex poisoned") = Some(value);
    }
}

/// Header rows the viewer renders above the scrollable body: the command
/// line, the status line, and a blank separator.
const HEADER_ROWS: usize = 3;

/// Read-only, scrollable viewer that tails one background bash task.
pub struct TaskOutputComponent {
    registry: TaskRegistry,
    id: TaskId,
    /// Command line, shown in the header for context.
    command: String,

    // --- spill-file tailing ---
    /// Spill path, resolved from the first registry snapshot that
    /// carries one.
    spill_path: Option<PathBuf>,
    /// Open read handle once the spill exists. We keep reading past EOF
    /// as the bash tool appends, which works because a regular file has
    /// no sticky EOF: a later read returns the bytes written since.
    spill: Option<File>,
    /// Bytes of a trailing line not yet terminated by a newline.
    pending: Vec<u8>,
    /// Completed logical lines (newline stripped), append-only.
    lines: Vec<String>,
    /// Wrapped visual rows for `lines`, cached at `wrap_width`. Rebuilt
    /// only when the width changes; otherwise we wrap just the lines
    /// appended since the last frame.
    visual: Vec<String>,
    /// Number of logical lines already reflected in `visual`.
    wrapped_count: usize,
    /// Width `visual` was wrapped at.
    wrap_width: usize,

    // --- live status snapshot ---
    status: TaskStatus,
    total_bytes: u64,

    // --- view state ---
    /// Index of the top visible visual row.
    scroll: usize,
    /// Stick to the bottom as new output arrives (`tail -f` behavior).
    /// Set on open and re-enabled by jump-to-bottom; any manual scroll
    /// up clears it.
    follow: bool,
    /// Body rows available this frame, derived from the window's inner
    /// height via [`Component::set_available_height`].
    viewport: usize,
    outcome: TaskOutputOutcomeHandle,
    focused: bool,
}

impl TaskOutputComponent {
    /// Build a viewer for task `id`, reading its output through `registry`.
    /// `command` is the task's command line, shown in the header.
    pub fn new(registry: TaskRegistry, id: TaskId, command: String) -> Self {
        let mut viewer = Self {
            registry,
            id,
            command,
            spill_path: None,
            spill: None,
            pending: Vec::new(),
            lines: Vec::new(),
            visual: Vec::new(),
            wrapped_count: 0,
            wrap_width: 0,
            status: TaskStatus::Running,
            total_bytes: 0,
            scroll: 0,
            follow: true,
            viewport: 10,
            outcome: TaskOutputOutcomeHandle::new(),
            focused: true,
        };
        // Load whatever has already been produced so the first frame
        // shows real content rather than an empty box.
        viewer.refresh();
        viewer
    }

    /// Hand the host a clone of the outcome slot.
    pub fn outcome_handle(&self) -> TaskOutputOutcomeHandle {
        self.outcome.clone()
    }

    /// Pull the live status and byte totals from the registry, then drain
    /// any new spill bytes into the line buffer.
    fn refresh(&mut self) {
        if let Some((status, read)) = self.registry.read(self.id) {
            self.status = status;
            self.total_bytes = read.stdout_total_bytes + read.stderr_total_bytes;
            if self.spill_path.is_none() {
                self.spill_path = read.spill_path;
            }
        }
        // A task evicted from the registry keeps its last-known buffer
        // and status; nothing more will arrive.
        self.pump_spill();
    }

    /// Read newly written spill bytes and split completed lines out.
    fn pump_spill(&mut self) {
        let Some(path) = self.spill_path.clone() else {
            return;
        };
        if self.spill.is_none() {
            // The spill is persisted up front for background tasks, but
            // tolerate a brief window where it is not yet openable and
            // retry on the next frame.
            match File::open(&path) {
                Ok(file) => self.spill = Some(file),
                Err(_) => return,
            }
        }
        let Some(file) = self.spill.as_mut() else {
            return;
        };
        let mut buf = [0u8; 64 * 1024];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.pending.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        self.absorb_lines();
    }

    /// Move newline-terminated lines from `pending` into `lines`, leaving
    /// any trailing partial line buffered for the next read.
    fn absorb_lines(&mut self) {
        let mut start = 0;
        while let Some(rel) = self.pending[start..].iter().position(|&b| b == b'\n') {
            let end = start + rel;
            self.lines.push(decode_line(&self.pending[start..end]));
            start = end + 1;
        }
        if start > 0 {
            self.pending.drain(..start);
        }
    }

    /// Ensure `visual` reflects every line in `lines` wrapped to `width`.
    fn ensure_wrapped(&mut self, width: usize) {
        if width != self.wrap_width {
            self.visual.clear();
            self.wrap_width = width;
            self.wrapped_count = 0;
        }
        for line in &self.lines[self.wrapped_count..] {
            self.visual.extend(wrap_text_with_ansi(line, width));
        }
        self.wrapped_count = self.lines.len();
    }

    /// Visual rows for the not-yet-terminated trailing line, wrapped fresh
    /// each frame (it is provisional, so it never enters `visual`).
    fn pending_rows(&self, width: usize) -> Vec<String> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let line = decode_line(&self.pending);
        if line.is_empty() {
            return Vec::new();
        }
        wrap_text_with_ansi(&line, width)
    }

    /// The command line, dimmed and truncated to the box width.
    fn command_line(&self, width: usize) -> String {
        style::dim(&truncate_to_width(&self.command, width, "…", false))
    }

    /// The status line: glyph + status word + size on the left, scroll
    /// position (or `following`) on the right, right-aligned.
    fn status_line(&self, width: usize, total: usize, viewport: usize) -> String {
        let (glyph, word) = status_glyph_word(self.status);
        let glyph = match self.status {
            TaskStatus::Running => style::dim(glyph),
            TaskStatus::Exited(Some(0)) => style::green(glyph),
            _ => style::red(glyph),
        };
        let left = format!(
            "{glyph} {} · {}",
            style::dim(&word),
            style::dim(&human_bytes(self.total_bytes)),
        );

        let right = if self.follow {
            "following".to_string()
        } else if total > viewport {
            let bottom = (self.scroll + viewport).min(total);
            format!("{}-{} / {}", self.scroll + 1, bottom, total)
        } else {
            String::new()
        };
        let right = style::dim(&right);

        let left_w = visible_width(&left);
        let right_w = visible_width(&right);
        if right_w == 0 {
            return left;
        }
        if left_w + right_w + 1 <= width {
            let gap = width - left_w - right_w;
            format!("{left}{}{right}", " ".repeat(gap))
        } else {
            // Too narrow for both: keep the status, drop the indicator.
            left
        }
    }

    /// Scroll up by `rows`, dropping follow mode. The upper bound is
    /// clamped in `render` against the live row total.
    fn scroll_up(&mut self, rows: usize) {
        self.follow = false;
        self.scroll = self.scroll.saturating_sub(rows);
    }

    /// Scroll down by `rows`, dropping follow mode.
    fn scroll_down(&mut self, rows: usize) {
        self.follow = false;
        self.scroll = self.scroll.saturating_add(rows);
    }
}

/// Glyph and human-readable word for a task status, matching the agent
/// picker's task-row glyphs.
fn status_glyph_word(status: TaskStatus) -> (&'static str, String) {
    match status {
        TaskStatus::Running => ("…", "running".to_string()),
        TaskStatus::Exited(Some(0)) => ("✓", "exited 0".to_string()),
        TaskStatus::Exited(Some(code)) => ("✗", format!("exited {code}")),
        TaskStatus::Exited(None) => ("✗", "signalled".to_string()),
        TaskStatus::Killed => ("✗", "killed".to_string()),
    }
}

/// Decode raw spill bytes into a display line: lossy UTF-8 plus a crude
/// carriage-return collapse.
fn decode_line(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    render_carriage_returns(&text).to_string()
}

/// Approximate a terminal's handling of bare carriage returns: a `\r`
/// returns the cursor to column 0, so what stays visible is the text
/// after the last `\r`. We also drop a single trailing `\r` left behind
/// when a `\r\n` line ending is split on the `\n`.
///
/// NOTE: this is an approximation. A real terminal overwrites in place,
/// so trailing characters from a longer earlier segment would remain. We
/// take the final segment instead, which is correct for the common
/// progress-bar case and good enough otherwise.
fn render_carriage_returns(s: &str) -> &str {
    let s = s.strip_suffix('\r').unwrap_or(s);
    s.rsplit('\r').next().unwrap_or(s)
}

/// Format a byte count as `B` / `KB` / `MB` / `GB`.
// Lossy `u64 as f64` is fine here: these values are small display sizes
// and a fractional rounding error in a human-readable count is harmless.
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

impl Component for TaskOutputComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        self.refresh();
        self.ensure_wrapped(width);

        let pending_rows = self.pending_rows(width);
        let total = self.visual.len() + pending_rows.len();
        let viewport = self.viewport.max(1);
        let max_scroll = total.saturating_sub(viewport);
        // Follow pins to the bottom; otherwise keep the user's position
        // but never past the end as the buffer shrinks on a width change.
        self.scroll = if self.follow {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };

        let mut out = Vec::with_capacity(HEADER_ROWS + viewport);
        out.push(self.command_line(width));
        out.push(self.status_line(width, total, viewport));
        out.push(String::new());

        let end = (self.scroll + viewport).min(total);
        for i in self.scroll..end {
            if i < self.visual.len() {
                out.push(self.visual[i].clone());
            } else {
                out.push(pending_rows[i - self.visual.len()].clone());
            }
        }
        out
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();
        if kb.matches(event, "tui.select.cancel") || kb.matches(event, "tui.input.submit") {
            self.outcome.set(TaskOutputOutcome::Closed);
            return true;
        }
        if kb.matches(event, ACTION_TASK_KILL) {
            drop(kb);
            // Kill in place; the status flip arrives via the event pump
            // and repaints the header. No-op once the task is terminal.
            if self.status == TaskStatus::Running {
                self.registry.kill(self.id);
            }
            return true;
        }
        if kb.matches(event, "tui.select.up") {
            drop(kb);
            self.scroll_up(1);
            return true;
        }
        if kb.matches(event, "tui.select.down") {
            drop(kb);
            self.scroll_down(1);
            return true;
        }
        if kb.matches(event, "tui.select.pageUp") {
            drop(kb);
            self.scroll_up(self.viewport);
            return true;
        }
        if kb.matches(event, "tui.select.pageDown") {
            drop(kb);
            self.scroll_down(self.viewport);
            return true;
        }
        drop(kb);

        // Vim-style extras the keybinding actions don't cover.
        match event.as_char() {
            Some('k') => self.scroll_up(1),
            Some('j') => self.scroll_down(1),
            Some(' ') => self.scroll_down(self.viewport),
            Some('g') => {
                self.scroll = 0;
                self.follow = false;
            }
            Some('G') => self.follow = true,
            Some('f') => self.follow = !self.follow,
            _ => {}
        }
        // Capturing overlay: swallow every other key so it never leaks to
        // background components.
        true
    }

    fn set_available_height(&mut self, rows: usize) {
        self.viewport = rows.saturating_sub(HEADER_ROWS).max(1);
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use aj_agent::events::AgentId;
    use aj_agent::tool::{TaskKind, TaskOutputSource, TaskRead};
    use aj_tui::keys::Key;
    use tempfile::NamedTempFile;

    use super::*;

    /// Fake output source returning a fixed snapshot, so the viewer can
    /// resolve a spill path and byte totals from the registry.
    struct FakeSource {
        read: TaskRead,
    }

    impl TaskOutputSource for FakeSource {
        fn snapshot(&self) -> TaskRead {
            self.read.clone()
        }
    }

    /// Build a viewer over a spill file pre-filled with `contents`. The
    /// returned `NamedTempFile` must be kept alive for the test's
    /// duration so the spill isn't unlinked.
    fn viewer(
        contents: &str,
        status: TaskStatus,
    ) -> (TaskOutputComponent, TaskRegistry, TaskId, NamedTempFile) {
        crate::config::keybindings::install_global_manager_defaults();
        let mut file = NamedTempFile::new().expect("temp spill");
        file.write_all(contents.as_bytes()).expect("write spill");
        file.flush().expect("flush spill");
        let read = TaskRead {
            spill_path: Some(file.path().to_path_buf()),
            stdout_total_bytes: u64::try_from(contents.len()).expect("spill length fits u64"),
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
        let mut v = TaskOutputComponent::new(registry.clone(), id, "echo hi".to_string());
        // Inner height of 8 → viewport of 5 body rows.
        v.set_available_height(8);
        (v, registry, id, file)
    }

    fn body(lines: &[String]) -> String {
        lines[HEADER_ROWS..].join("\n")
    }

    #[test]
    fn opens_following_at_the_bottom() {
        let contents: String = (1..=20).map(|n| format!("line{n}\n")).collect();
        let (mut v, _r, _id, _f) = viewer(&contents, TaskStatus::Running);
        let lines = v.render(40);
        let shown = body(&lines);
        assert!(shown.contains("line20"), "want bottom: {shown}");
        assert!(
            !shown.contains("line1\n"),
            "top should be scrolled off: {shown}"
        );
        // The header advertises follow mode.
        assert!(lines[1].contains("following"), "header: {:?}", lines[1]);
    }

    #[test]
    fn jump_to_top_then_bottom() {
        let contents: String = (1..=20).map(|n| format!("line{n}\n")).collect();
        let (mut v, _r, _id, _f) = viewer(&contents, TaskStatus::Running);

        assert!(v.handle_input(&Key::char('g')));
        let top = body(&v.render(40));
        assert!(top.contains("line1"), "want top: {top}");
        assert!(
            !top.contains("line20"),
            "bottom should be off-screen: {top}"
        );

        assert!(v.handle_input(&Key::char('G')));
        let bottom = body(&v.render(40));
        assert!(bottom.contains("line20"), "want bottom: {bottom}");
    }

    #[test]
    fn renders_unterminated_trailing_line() {
        let (mut v, _r, _id, _f) = viewer("done\nworking", TaskStatus::Running);
        let shown = body(&v.render(40));
        assert!(shown.contains("done"), "{shown}");
        assert!(
            shown.contains("working"),
            "partial line should show: {shown}"
        );
    }

    #[test]
    fn terminal_status_shows_in_header() {
        let (mut v, _r, _id, _f) = viewer("out\n", TaskStatus::Exited(Some(0)));
        let lines = v.render(40);
        assert!(lines[1].contains("exited 0"), "header: {:?}", lines[1]);
        assert!(lines[1].contains('✓'), "header glyph: {:?}", lines[1]);
    }

    #[test]
    fn enter_and_esc_close() {
        let (mut v, _r, _id, _f) = viewer("x\n", TaskStatus::Running);
        let handle = v.outcome_handle();
        v.handle_input(&Key::escape());
        assert!(matches!(handle.take(), Some(TaskOutputOutcome::Closed)));
        v.handle_input(&Key::enter());
        assert!(matches!(handle.take(), Some(TaskOutputOutcome::Closed)));
    }

    #[test]
    fn carriage_returns_collapse_to_last_segment() {
        assert_eq!(render_carriage_returns("10%\r50%\r100%"), "100%");
        assert_eq!(render_carriage_returns("plain"), "plain");
        // A trailing CR (from a CRLF split on LF) is dropped.
        assert_eq!(render_carriage_returns("text\r"), "text");
    }

    #[test]
    fn human_bytes_spans_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0 MB");
    }
}
