//! Tool-execution component.
//!
//! Renders one tool call's lifecycle: the `name(arguments)` header
//! that appears as soon as the agent emits `ToolExecutionStart`,
//! optional progress updates, and the final structured result
//! when `ToolExecutionEnd` fires. Every tool variant flows through
//! this single component; the body rendering switches on the
//! [`ToolDetails`] variant. Specialised rendering helpers for
//! `Bash` and `Diff` live in [`super::bash_execution`] and
//! [`super::diff`] respectively.
//!
//! Visually the component renders as a bubble: a coloured
//! rectangle painted with one of three background tints depending
//! on the current [`Status`] (pending → neutral, succeeded →
//! greenish, failed → reddish). The header and body lines sit
//! inside the bubble at `padding_x = 1`, framed by one bg-painted
//! blank row above and below (`padding_y = 1`). Separation from
//! the surrounding chat elements comes from the auto-spacer the
//! [`crate::modes::interactive::event_pump::EventPump`] inserts
//! between sibling chat-container children.
//!
//! ## Render-cost shape
//!
//! The bubble's frame, padding and background paint live in an
//! owned [`aj_tui::components::text_box::TextBox`]; the header and
//! body each live in their own [`aj_tui::components::text::Text`]
//! child inside that box. Both kinds of widget cache their
//! rendered output keyed on inputs that change rarely
//! (`(text, width)` for `Text`; `(width, child-lines, bg-sample)`
//! for `TextBox`), so a finalised tool with kilobytes of body
//! costs one wrap pass at construction and then string-equality
//! checks on every subsequent frame. The hot render path in a
//! long chat with many finished tools is dominated by clones of
//! the cached `Vec<String>`, not by re-wrapping.
//!
//! Mutating callers (`update_partial`, `update_result`) tear the
//! `Text` children down and rebuild them so the next render
//! repopulates the caches with the new content. State that
//! doesn't affect visible output (status flip to `Succeeded` /
//! `Failed`) still rebuilds because the header glyph changes; the
//! body is dropped and re-installed in that path too, which is
//! fine because state changes are rare compared to renders.
//!
//! See `docs/aj-next-plan.md` §1.2 (`ToolDetails`) and §4
//! (`components/tool_execution.rs`).

use std::any::Any;
use std::sync::Arc;

use aj_agent::tool::{TaskStatus, ToolDetails};
use aj_models::types::{ImageContent, UserContent};
use aj_tools::sanitize_terminal_output;
use aj_tui::ansi::wrap_text_with_ansi;
use aj_tui::capabilities::{ImageProtocol, get_capabilities};
use aj_tui::component::Component;
use aj_tui::components::image::Image;
use aj_tui::components::text::Text;
use aj_tui::components::text_box::TextBox;
use aj_tui::keys::InputEvent;
use aj_tui::style;
use serde_json::Value;

use crate::config::theme::ChatTheme;
use crate::modes::interactive::components::bash_execution::render_bash_body;
use crate::modes::interactive::components::diff::render_unified_diff;
use crate::modes::interactive::render_settings::RenderSettings;

/// Horizontal padding inside the bubble (one column on each side
/// so the tinted rectangle reads as an inset block rather than
/// edge-to-edge text).
const PADDING_X: usize = 1;
/// Vertical padding inside the bubble. One bg-painted blank row
/// above and below the content matches the user-message bubble's
/// rhythm so the two kinds of bubbles compose cleanly when the
/// auto-spacer drops a plain blank line between them.
const PADDING_Y: usize = 1;

/// Maximum body lines rendered for a head-truncated tool output
/// (`Text` / `SubAgentReport` variants) when collapsed. Sized to
/// give a quick at-a-glance preview without flooding the
/// scrollback; revealing the rest is one `aj.tools.expand`
/// keystroke away.
const TEXT_COLLAPSED_LINES: usize = 10;

/// Maximum body lines rendered for a head-truncated
/// `SubAgentReport` body when collapsed. Aliased to
/// [`TEXT_COLLAPSED_LINES`] for parallelism but kept as its own
/// constant so a future divergence is a one-line change.
const REPORT_COLLAPSED_LINES: usize = TEXT_COLLAPSED_LINES;

/// Whether the user-visible hint that names the expansion key
/// should be styled to describe head- or tail-truncated content.
/// The phrasing (`N more lines` vs `N earlier lines`) keeps the
/// hint honest about which end of the stream got dropped.
#[derive(Clone, Copy, Debug)]
pub(crate) enum HintKind {
    /// Body was truncated to its head; the hidden lines come after
    /// the visible ones (`Text`, `SubAgentReport`).
    More,
    /// Body was truncated to its tail; the hidden lines come
    /// before the visible ones (`Bash` per-stream).
    Earlier,
}

/// Format the `… (N <kind> lines, <key> to expand)` hint line
/// that signals a collapsed tool body has more content. The key is
/// resolved and display-formatted at call time through the global
/// keybindings registry; when no key is bound to
/// [`crate::config::keybindings::ACTION_TOOLS_EXPAND`] the hint omits
/// the parenthetical key reference rather than advertising an action
/// the user can't trigger. The returned string is `style::dim`-wrapped
/// so it reads as muted next to the surrounding body content.
pub(crate) fn expand_hint(more: usize, kind: HintKind) -> String {
    let kb = aj_tui::keybindings::get();
    let keys = kb.get_keys(crate::config::keybindings::ACTION_TOOLS_EXPAND);
    let key = keys
        .first()
        .map(|k| aj_tui::keybindings::format_keybinding(k));
    style::dim(&format_expand_hint(more, kind, key.as_deref()))
}

/// Pure formatting half of [`expand_hint`]; takes the display-formatted
/// key (or `None` for the no-binding fallback) so it stays unit-testable
/// without touching the process-wide keybindings registry.
fn format_expand_hint(more: usize, kind: HintKind, key: Option<&str>) -> String {
    let word = match kind {
        HintKind::More => "more",
        HintKind::Earlier => "earlier",
    };
    match key {
        Some(key) => format!("… ({more} {word} lines, {key} to expand)"),
        None => format!("… ({more} {word} lines)"),
    }
}

/// Minimum total render width at which the bubble framing kicks
/// in. Below this we fall back to a plain header+body listing so
/// the bg-padding pipeline (which assumes at least two cells of
/// horizontal padding plus one cell of content) doesn't try to
/// paint a degenerate row. The threshold also covers the headless
/// `width = 0` case used by some tests.
const MIN_BUBBLE_WIDTH: usize = 3;

/// Lifecycle states a tool execution moves through. Drives the
/// rendered status indicator in the header line and the bubble's
/// background tint so users can distinguish a not-yet-started call
/// from a finished one at a glance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    /// Tool args have been received but the tool body hasn't run
    /// yet (we observed `ToolExecutionStart`). Header shows a
    /// dim spinner glyph; the bubble paints with the neutral
    /// pending tint.
    Started,
    /// The tool finished and the viewer should read it as a
    /// success: header shows a green check, the bubble paints
    /// with the success tint. Maps from `is_error: false` *and*
    /// (for bash) a zero or unknown exit code. See
    /// [`derive_status`] for the full rule.
    Succeeded,
    /// The tool finished and the viewer should read it as a
    /// failure: header shows a red cross, the bubble paints
    /// with the error tint. Maps from `is_error: true` or (for
    /// bash) a non-zero exit code.
    Failed,
}

/// On-screen representation of a single tool call.
///
/// One component per `tool_use_id`. The event pump keys these by
/// id in a `HashMap<String, usize>` so each
/// `ToolExecutionStart` / `ToolExecutionUpdate` / `ToolExecutionEnd`
/// event reaches the right component. The component is a `Box<dyn
/// Component>` from the chat container's perspective; the event
/// pump downcasts via `as_any_mut` to call mutating methods.
pub struct ToolExecutionComponent {
    /// Tool name (`bash`, `read_file`, …) shown in the header.
    tool_name: String,
    /// JSON-encoded arguments, rendered inside the parens after
    /// the name. Stored as a string so we can re-render cheaply.
    args_pretty: String,
    /// Current execution status. Drives the header glyph and the
    /// bubble background tint.
    status: Status,
    /// Body lines rendered under the header. Populated from the
    /// `ToolDetails` variant on every `ToolExecutionUpdate` and
    /// finalized on `ToolExecutionEnd`.
    body: Vec<String>,
    /// Most recent details payload passed to [`Self::update_partial`]
    /// or [`Self::update_result`]. Retained so
    /// [`Self::reconcile_settings`] can re-run [`render_details_body`]
    /// with the new expansion state at render time without needing
    /// the event pump to replay the original event. `None` between
    /// construction and the first update.
    last_details: Option<ToolDetails>,
    /// `is_error` value paired with [`Self::last_details`]; only
    /// meaningful when `last_details` was set by
    /// [`Self::update_result`]. Used so re-rendering the body for
    /// an expansion toggle doesn't accidentally flip the status
    /// back to `Started`.
    last_is_error: bool,
    /// Last-applied body render mode — the mode the cached
    /// [`Self::body`] reflects. Kept in step with
    /// `settings.tools_expanded()` by [`Self::reconcile_settings`]:
    /// `true` renders the full content, `false` the compact
    /// head/tail-truncated form. Orthogonal to [`Self::header_only`].
    expanded: bool,
    /// Whether to render only the header line, with no bubble,
    /// background, or body. Set when this tool call lives inside a
    /// sub-agent box, whose own background paints behind it.
    /// Per-component (driven by the owning box's mode), not a
    /// session-wide setting; orthogonal to [`Self::expanded`].
    header_only: bool,
    /// Shared session-wide render settings. Read at render time
    /// (see [`Self::reconcile_settings`]) so toggling tool
    /// expansion reaches this component without the toggle site
    /// walking the transcript.
    settings: RenderSettings,
    /// Generation of [`Self::settings`] that the cached body and
    /// children reflect. A mismatch at render time triggers a
    /// reconcile.
    last_generation: u64,
    /// Bg-paint closure for the in-flight state. Stored once at
    /// construction so the render path never has to rebuild the
    /// closure.
    bg_pending: Arc<dyn Fn(&str) -> String>,
    /// Bg-paint closure for the `Succeeded` state.
    bg_success: Arc<dyn Fn(&str) -> String>,
    /// Bg-paint closure for the `Failed` state.
    bg_error: Arc<dyn Fn(&str) -> String>,
    /// Cached child tree.
    ///
    /// The bubble owns a `TextBox` whose padding becomes the
    /// bubble's frame (1 cell horizontally, 1 row vertically) and
    /// whose `bg_fn` reflects the current [`Status`]. Inside it
    /// sit two zero-padding `Text` children: a header text and an
    /// optional body text. Both `Text` and `TextBox` carry their
    /// own caches, so steady-state renders return cached vectors
    /// without re-running the wrap pass.
    ///
    /// `rebuild_children` is the single mutating path: it tears
    /// down the existing children and reinstalls fresh ones, so
    /// `Text`'s `(text, width)` cache and `TextBox`'s
    /// `(child-lines, width, bg-sample)` cache both re-populate on
    /// the next render.
    bubble: TextBox,
    /// Per-cell pixel size sourced from
    /// [`aj_tui::terminal::Terminal::cell_pixel_size`] at the time
    /// the component was constructed. Threaded down to the inline
    /// [`Image`] child so the image footprint lines up with the
    /// surrounding cell grid. `None` means the terminal didn't
    /// report a size; [`Image`] falls back to a sensible default.
    cell_pixel_size: Option<(u32, u32)>,
    /// Inline image payload (base64, mime, source pixels) when
    /// the tool result carried a [`UserContent::Image`] block AND
    /// the host terminal advertises an image protocol. Set by
    /// [`Self::update_result`] / [`Self::update_partial`] and
    /// consumed by [`Self::rebuild_children`] to drop a fresh
    /// [`Image`] child into the bubble. Only the first image
    /// block on a multi-image result is kept — current tools
    /// (`read_file`) emit at most one image per call.
    image_payload: Option<ImagePayload>,
    /// Last-applied inline-image render mode — whether to render an
    /// inline [`Image`] child when the terminal advertises an image
    /// protocol. `false` falls back to the textual placeholder
    /// `render_details_body` already produces. Kept in step with
    /// `settings.show_image_in_terminal()` by
    /// [`Self::reconcile_settings`]; sourced from the
    /// `image_show_in_terminal` config key.
    show_image_in_terminal: bool,
    /// Terminal status of the background task this cell launched,
    /// set by [`Self::finish_task`] when the task's `TaskEnd`
    /// arrives. Renders into the header's task badge and freezes
    /// the cell: once set, further partial updates are ignored.
    /// Always `None` for foreground tool calls and for resumed
    /// transcripts (task events are transient, so a replayed launch
    /// cell keeps its started snapshot and plain `[task #N]` badge).
    task_status: Option<TaskStatus>,
}

/// Snapshot of the inline image attachment to render alongside
/// the tool's body, captured from the result's wire content.
#[derive(Clone)]
struct ImagePayload {
    base64_data: String,
    mime_type: String,
    pixels: (u32, u32),
    /// File / source path for the inline image. Surfaced as
    /// iTerm2's OSC 1337 `name=` field and in the textual
    /// fallback. Sourced from `ToolDetails::Image.summary`,
    /// which `read_file` populates with the display path of the
    /// source image. `None` when the summary is empty.
    filename: Option<String>,
}

impl ToolExecutionComponent {
    /// Build a new component for a tool call with the given name
    /// and arguments. The component starts in [`Status::Started`]
    /// and paints with the `tool_pending_bg` tint until the agent
    /// emits a result; [`Self::update_partial`] / [`Self::update_result`]
    /// flip the status and the matching bg. `settings` is the
    /// shared session-wide render config; the component snapshots
    /// the current expansion / inline-image modes and re-reads
    /// `settings` at render time when the generation moves.
    pub fn new(
        tool_name: String,
        args: &Value,
        theme: &ChatTheme,
        settings: RenderSettings,
    ) -> Self {
        Self::with_cell_pixel_size(tool_name, args, theme, settings, None)
    }

    /// Construct a component with an explicit per-cell pixel size
    /// for the inline [`Image`] child. Threaded through from the
    /// event pump so the [`Tui`]'s terminal-backed cell size
    /// flows into the image protocol's footprint math. A `None`
    /// argument is equivalent to [`Self::new`] and means
    /// [`Image`] should use [`aj_tui::image_protocol::DEFAULT_CELL_PIXEL_SIZE`].
    pub fn with_cell_pixel_size(
        tool_name: String,
        args: &Value,
        theme: &ChatTheme,
        settings: RenderSettings,
        cell_pixel_size: Option<(u32, u32)>,
    ) -> Self {
        let expanded = settings.tools_expanded();
        let show_image_in_terminal = settings.show_image_in_terminal();
        let last_generation = settings.generation();
        let mut me = Self {
            tool_name,
            args_pretty: format_args(args),
            status: Status::Started,
            body: Vec::new(),
            last_details: None,
            last_is_error: false,
            expanded,
            header_only: false,
            settings,
            last_generation,
            bg_pending: Arc::clone(&theme.tool_pending_bg),
            bg_success: Arc::clone(&theme.tool_success_bg),
            bg_error: Arc::clone(&theme.tool_error_bg),
            bubble: TextBox::new(PADDING_X, PADDING_Y),
            cell_pixel_size,
            image_payload: None,
            show_image_in_terminal,
            task_status: None,
        };
        me.bubble.set_bg_fn(me.make_bg_box());
        me.rebuild_children();
        me
    }

    /// Replace the rendered body with the partial snapshot in
    /// `details`. Called from
    /// [`aj_agent::events::AgentEvent::ToolExecutionUpdate`] for
    /// foreground progress and from
    /// [`aj_agent::events::AgentEvent::TaskOutput`] for a background
    /// task's live tail (which keeps flowing after the launch call's
    /// `ToolExecutionEnd` — the cell deliberately accepts updates
    /// past finalization for that reason).
    pub fn update_partial(&mut self, details: &ToolDetails, content: &[UserContent]) {
        // Frozen: the background task reached its terminal status,
        // so a straggling snapshot must not reopen the cell.
        if self.task_status.is_some() {
            return;
        }
        self.reconcile_settings();
        self.body = render_details_body(details, self.expanded);
        self.image_payload = derive_image_payload(details, content);
        self.last_details = Some(details.clone());
        self.last_is_error = false;
        self.rebuild_children();
    }

    /// Finalize the component with the tool's result. Called from
    /// [`aj_agent::events::AgentEvent::ToolExecutionEnd`].
    ///
    /// `content` is the wire content vector from the tool result;
    /// it is currently only consulted to decide whether an inline
    /// image (carried as a [`aj_models::types::UserContent::Image`]
    /// block) should be rendered. Tools that don't return images
    /// pass an empty slice. When multiple image blocks are present
    /// only the first is rendered for now (no current tool returns
    /// more than one).
    pub fn update_result(
        &mut self,
        details: &ToolDetails,
        content: &[UserContent],
        is_error: bool,
    ) {
        self.reconcile_settings();
        self.body = render_details_body(details, self.expanded);
        self.image_payload = derive_image_payload(details, content);
        self.last_details = Some(details.clone());
        self.last_is_error = is_error;
        self.status = derive_status(details, is_error);
        // The status flip pulls a different bg closure into the
        // box. `set_bg_fn` doesn't invalidate the cache itself;
        // `TextBox::render` compares a sampled application of the
        // new closure against the cached sample, so a real bg
        // change reaches the screen on the next render even though
        // the children are unchanged.
        self.bubble.set_bg_fn(self.make_bg_box());
        self.rebuild_children();
    }

    /// Pull the latest shared render settings into this component's
    /// last-applied fields and snapshot the generation.
    ///
    /// Returns `true` when a setting that affects the rendered body
    /// or children — tool expansion or inline-image mode — actually
    /// changed, so the caller knows to rebuild those caches. A no-op
    /// for the caches when nothing relevant changed, so an unrelated
    /// toggle (e.g. thinking fold, which shares the generation
    /// counter) doesn't churn the cached `Text` / `TextBox`
    /// children.
    fn reconcile_settings(&mut self) -> bool {
        self.last_generation = self.settings.generation();
        let mut changed = false;
        let expanded = self.settings.tools_expanded();
        if self.expanded != expanded {
            self.expanded = expanded;
            changed = true;
        }
        let show_image = self.settings.show_image_in_terminal();
        if self.show_image_in_terminal != show_image {
            self.show_image_in_terminal = show_image;
            changed = true;
        }
        changed
    }

    /// Render only the header line (no bubble, no background, no body).
    /// Used when this tool call lives inside a sub-agent box, whose own
    /// background paints behind it. Orthogonal to `expanded`.
    pub fn set_header_only(&mut self, value: bool) {
        if self.header_only == value {
            return;
        }
        self.header_only = value;
        self.invalidate();
    }

    /// Freeze a background-task cell with the task's terminal
    /// status: the header badge gains the status text, the bubble
    /// repaints per the outcome, and further partial updates are
    /// ignored. Called from
    /// [`aj_agent::events::AgentEvent::TaskEnd`].
    pub fn finish_task(&mut self, status: TaskStatus) {
        self.task_status = Some(status);
        self.status = match status {
            TaskStatus::Exited(Some(0)) => Status::Succeeded,
            // `Running` can't legitimately reach a TaskEnd; keep the
            // current paint rather than inventing an outcome.
            TaskStatus::Running => self.status,
            TaskStatus::Exited(_) | TaskStatus::Killed => Status::Failed,
        };
        self.bubble.set_bg_fn(self.make_bg_box());
        self.rebuild_children();
    }

    /// Render the header line (`status tool(args) [task #N]`). Kept
    /// private so the header style is uniform across every variant.
    fn header_line(&self) -> String {
        let glyph = match self.status {
            Status::Started => style::dim("…"),
            Status::Succeeded => style::green("✓"),
            Status::Failed => style::red("✗"),
        };
        let name = style::bold(&self.tool_name);
        let mut line = format!("{glyph} {name}({})", style::dim(&self.args_pretty));
        if let Some(badge) = self.task_badge() {
            line.push(' ');
            line.push_str(&style::dim(&badge));
        }
        line
    }

    /// The header's background-task badge: `[task #N]` while the
    /// task runs (or for a resumed launch cell, where the terminal
    /// status is unknown), `[task #N · exited 0]` etc. once
    /// [`Self::finish_task`] recorded the outcome. `None` for
    /// foreground tool calls — the task id rides on the
    /// [`ToolDetails::Bash`] payload, so its presence is what marks
    /// the cell as a background launch.
    fn task_badge(&self) -> Option<String> {
        let ToolDetails::Bash {
            task_id: Some(id), ..
        } = self.last_details.as_ref()?
        else {
            return None;
        };
        Some(match self.task_status {
            None | Some(TaskStatus::Running) => format!("[task #{id}]"),
            Some(TaskStatus::Exited(Some(code))) => format!("[task #{id} · exited {code}]"),
            Some(TaskStatus::Exited(None)) => format!("[task #{id} · terminated by signal]"),
            Some(TaskStatus::Killed) => format!("[task #{id} · killed]"),
        })
    }

    /// Build the boxed bg closure that matches the current status.
    ///
    /// The returned `Box<dyn Fn>` clones an [`Arc`] off the stored
    /// status closures and dispatches through it. Cloning the Arc
    /// keeps the bubble's closure pointing at the same underlying
    /// function as the rest of the chat theme, so calling sites
    /// that share a theme also share their bg paint cache lines.
    fn make_bg_box(&self) -> Box<dyn Fn(&str) -> String> {
        let arc = match self.status {
            Status::Started => Arc::clone(&self.bg_pending),
            Status::Succeeded => Arc::clone(&self.bg_success),
            Status::Failed => Arc::clone(&self.bg_error),
        };
        Box::new(move |s: &str| arc(s))
    }

    /// Tear down the bubble's children and rebuild them from the
    /// current header / body state.
    ///
    /// Called from every state-changing path (`new`,
    /// `update_partial`, `update_result`). `TextBox::clear`
    /// invalidates the bubble's frame cache, and each freshly
    /// constructed `Text` starts with an empty wrap cache, so the
    /// next render does the wrap pass once and caches it. Renders
    /// in between state changes return the cached output.
    fn rebuild_children(&mut self) {
        self.bubble.clear();

        // Header. Zero internal padding so the `TextBox`'s
        // 1-column inset is the only horizontal margin around it;
        // double-padding would push the header off the visual
        // grid the rest of the chat scrollback shares.
        let header_text = Text::new(&self.header_line(), 0, 0);
        self.bubble.add_child(Box::new(header_text));

        // Body. Joining with `\n` lets `wrap_text_with_ansi`
        // (called inside `Text::render`) see the body as one
        // multi-line input and track its ANSI state across the
        // implicit line breaks. The body helpers in this module
        // each emit fully self-terminated styled rows, so the
        // joined-vs-per-line wrap behaviour is identical for
        // today's inputs; the join is the simpler shape and
        // matches how `Text` already wraps the rest of the chat.
        if !self.body.is_empty() {
            let body_joined = self.body.join("\n");
            let body_text = Text::new(&body_joined, 0, 0);
            self.bubble.add_child(Box::new(body_text));
        }

        // Inline image attachment. Rendered only when the tool
        // result carried a [`UserContent::Image`] block AND the
        // host terminal advertises an image protocol AND the
        // user hasn't suppressed inline rendering via the
        // `image_show_in_terminal` config key. Kitty + non-PNG
        // falls back to the textual placeholder that
        // [`render_details_body`] already produces, because
        // Kitty doesn't accept JPEG inline.
        if let Some(payload) = &self.image_payload {
            let caps = get_capabilities();
            let inline_ok = self.show_image_in_terminal
                && match caps.images {
                    Some(ImageProtocol::Kitty) => payload.mime_type == "image/png",
                    Some(ImageProtocol::ITerm2) => true,
                    None => false,
                };
            if inline_ok {
                // `max_cells` keeps a huge image from taking
                // half the screen. Chosen to roughly match the
                // bubble's text body height.
                let mut image = Image::new(
                    payload.base64_data.clone(),
                    payload.mime_type.clone(),
                    payload.pixels,
                    (40, 20),
                    self.cell_pixel_size,
                );
                if let Some(name) = &payload.filename {
                    image = image.with_filename(name.clone());
                }
                self.bubble.add_child(Box::new(image));
            }
        }
    }
}

impl Component for ToolExecutionComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Pull in any session-wide settings change (tool expansion
        // or inline-image toggle) before painting. Gated on the
        // generation so steady-state renders skip the work. Done
        // before the header-only short-circuit so a tool inside a
        // collapsed sub-agent box keeps its body cache current and
        // shows the right content the instant the box switches to
        // the full view.
        if self.settings.generation() != self.last_generation && self.reconcile_settings() {
            if let Some(details) = self.last_details.clone() {
                self.body = render_details_body(&details, self.expanded);
            }
            self.rebuild_children();
        }

        // Header-only mode: just the wrapped header line, no bubble
        // or background, so the tool composes inside the sub-agent
        // box's own painted background.
        if self.header_only {
            return wrap_text_with_ansi(&self.header_line(), width.max(1));
        }

        // Tiny widths drop into a degraded "render plain" path so
        // the strict line-width check in `Tui::render` doesn't
        // trip on the bg-padding pipeline. Headless and zero-width
        // edge cases land here too. The bubble framing needs at
        // least one column on each side plus one column of content,
        // so anything below [`MIN_BUBBLE_WIDTH`] skips the box and
        // falls back to the original wrap-per-line path.
        if width < MIN_BUBBLE_WIDTH {
            let mut out = Vec::with_capacity(self.body.len() + 1);
            out.push(self.header_line());
            for line in &self.body {
                out.extend(wrap_text_with_ansi(line, width.max(1)));
            }
            return out;
        }

        // Steady-state hot path: delegates to the cached bubble.
        // First render after a state change rebuilds the caches;
        // every render in between is a `Vec<String>` clone out of
        // the box's cache.
        self.bubble.render(width)
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }

    fn invalidate(&mut self) {
        // Propagate down. `TextBox::invalidate` drops its own
        // cache and calls `invalidate` on each child `Text`, so
        // the next render rebuilds from scratch.
        self.bubble.invalidate();
    }
}

impl AsRef<dyn Any> for ToolExecutionComponent {
    fn as_ref(&self) -> &(dyn Any + 'static) {
        self
    }
}

/// Decide which [`Status`] to paint a finalized tool with.
///
/// The agent's `is_error` flag is reserved for catastrophic
/// failures (tool cancellation, timeout, JSON parse errors). It
/// deliberately does *not* fire for a successful invocation that
/// happens to return a non-zero exit code: per the bash tool's
/// contract the model can read the `exit_code` field of the
/// `ToolDetails::Bash` payload and decide what to do, and we don't
/// want to second-guess that for the model's input view.
///
/// For the *human* viewer, though, a `[exit 1]` line is plainly a
/// failure and a green ✓ on the same bubble reads wrong. So we
/// override here: any tool result that carries an explicit non-
/// zero exit code paints as [`Status::Failed`] regardless of the
/// agent-side flag. Other variants fall back to `is_error`.
fn derive_status(details: &ToolDetails, is_error: bool) -> Status {
    if is_error {
        return Status::Failed;
    }
    if let ToolDetails::Bash {
        exit_code: Some(code),
        ..
    } = details
    {
        if *code != 0 {
            return Status::Failed;
        }
    }
    Status::Succeeded
}

/// Build a single-line argument summary from the tool's input
/// JSON. The goal is a compact `command(arg1=val1, arg2=val2)`
/// preview that fits on one line; if the JSON is too verbose for
/// that, fall back to a `…` placeholder.
fn format_args(args: &Value) -> String {
    match args {
        Value::Object(map) => {
            let mut parts = Vec::with_capacity(map.len());
            for (k, v) in map {
                let v_str = match v {
                    Value::String(s) => format!("{k}={}", quote_for_summary(s)),
                    Value::Number(n) => format!("{k}={n}"),
                    Value::Bool(b) => format!("{k}={b}"),
                    Value::Null => format!("{k}=null"),
                    Value::Array(_) | Value::Object(_) => format!("{k}=…"),
                };
                parts.push(v_str);
            }
            parts.join(", ")
        }
        Value::String(s) => quote_for_summary(s),
        // Bare scalars or arrays go through the JSON form.
        other => other.to_string(),
    }
}

/// Wrap a free-form string in double quotes for the summary line.
/// Newlines / control characters are replaced with their `\n` /
/// `\t` escapes so the header stays on one row even when the input
/// happened to be multi-line.
fn quote_for_summary(s: &str) -> String {
    const MAX_INLINE: usize = 60;
    let cleaned = s
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r");
    if cleaned.chars().count() > MAX_INLINE {
        let head: String = cleaned.chars().take(MAX_INLINE).collect();
        format!("\"{head}…\"")
    } else {
        format!("\"{cleaned}\"")
    }
}

/// Pick out the first [`UserContent::Image`] block in the tool's
/// wire content and pair it with the source pixel dimensions
/// from the matching [`ToolDetails::Image`].
///
/// Returns `None` when `details` is not the `Image` variant or
/// `content` carries no image block. Today's tools (`read_file`)
/// emit at most one image per call; if a future tool produces
/// more we only render the first one, intentionally — multi-
/// image inline rendering is a separate layout problem.
fn derive_image_payload(details: &ToolDetails, content: &[UserContent]) -> Option<ImagePayload> {
    let ToolDetails::Image {
        summary,
        original_dimensions,
        displayed_dimensions,
        ..
    } = details
    else {
        return None;
    };
    let image = content.iter().find_map(|c| match c {
        UserContent::Image(img) => Some(img),
        UserContent::Text(_) => None,
    })?;
    // `displayed_dimensions` reflects any resize the tool
    // applied before encoding the bytes; it's the correct
    // pixel size for the base64 payload we're about to ship
    // to the terminal. `original_dimensions` is kept on the
    // details for the textual fallback's annotation line.
    let _ = original_dimensions;
    let ImageContent { data, mime_type } = image;
    let filename = (!summary.is_empty()).then(|| summary.clone());
    Some(ImagePayload {
        base64_data: data.clone(),
        mime_type: mime_type.clone(),
        pixels: *displayed_dimensions,
        filename,
    })
}

/// Render the body lines for a [`ToolDetails`] variant. Switching
/// here keeps each variant's specialised rendering close to the
/// component while letting [`Self::render`] stay variant-agnostic.
///
/// Every raw text field that originated outside this crate -- tool
/// summaries, command strings, sub-agent reports, the diff payload
/// -- passes through [`sanitize_terminal_output`] before any styling
/// is applied. Today the bash tool already sanitises its own stdout
/// / stderr at the source, so applying the transform a second time
/// here is a no-op for that path; the call covers every other variant
/// and guards against future tools that emit raw subprocess output
/// without going through the bash tool's helper. The transform
/// strips ANSI escapes, drops carriage returns, and removes other
/// terminal-control bytes that would otherwise disagree with the
/// renderer's width math (and produce a ragged right edge on the
/// surrounding bubble) or clobber adjacent cells via cursor moves
/// or erase-in-line side effects.
fn render_details_body(details: &ToolDetails, expanded: bool) -> Vec<String> {
    match details {
        ToolDetails::Text { summary, body } => {
            let summary = sanitize_terminal_output(summary);
            let body = sanitize_terminal_output(body);
            let mut lines = Vec::new();
            if !summary.is_empty() {
                lines.push(style::dim(&summary));
            }
            let mut body_lines: Vec<String> = body.split('\n').map(|s| s.to_string()).collect();
            // Trim a trailing empty line introduced by a body that
            // ended in `\n`; the surrounding bubble's bottom pad
            // already handles the vertical separation.
            if body_lines.last().is_some_and(|l| l.is_empty()) {
                body_lines.pop();
            }
            if !expanded && body_lines.len() > TEXT_COLLAPSED_LINES {
                let more = body_lines.len() - TEXT_COLLAPSED_LINES;
                body_lines.truncate(TEXT_COLLAPSED_LINES);
                lines.extend(body_lines);
                lines.push(expand_hint(more, HintKind::More));
            } else {
                lines.extend(body_lines);
            }
            lines
        }
        ToolDetails::Diff {
            path,
            before,
            after,
        } => render_unified_diff(
            &sanitize_terminal_output(path),
            &sanitize_terminal_output(before),
            &sanitize_terminal_output(after),
        ),
        ToolDetails::Bash {
            command,
            stdout,
            stderr,
            exit_code,
            truncated,
            full_output_path,
            stdout_truncation,
            stderr_truncation,
            task_id: _,
        } => {
            let command = sanitize_terminal_output(command);
            // `stdout` / `stderr` are already sanitised at the bash
            // tool source; running the transform again here is cheap
            // and keeps this match arm self-contained against future
            // changes to the bash payload's provenance.
            let stdout = sanitize_terminal_output(stdout);
            let stderr = sanitize_terminal_output(stderr);
            let mut lines = vec![style::dim(&format!("$ {command}"))];
            lines.extend(render_bash_body(
                &stdout,
                &stderr,
                *exit_code,
                *truncated,
                full_output_path.as_ref(),
                stdout_truncation.as_ref(),
                stderr_truncation.as_ref(),
                expanded,
            ));
            lines
        }
        ToolDetails::SubAgentReport {
            agent_id,
            task,
            report,
        } => {
            let task = sanitize_terminal_output(task);
            let report = sanitize_terminal_output(report);
            let mut lines = vec![style::dim(&format!("sub-agent {agent_id}: {task}"))];
            let mut report_lines: Vec<String> = report.split('\n').map(|s| s.to_string()).collect();
            if report_lines.last().is_some_and(|l| l.is_empty()) {
                report_lines.pop();
            }
            if !expanded && report_lines.len() > REPORT_COLLAPSED_LINES {
                let more = report_lines.len() - REPORT_COLLAPSED_LINES;
                report_lines.truncate(REPORT_COLLAPSED_LINES);
                lines.extend(report_lines);
                lines.push(expand_hint(more, HintKind::More));
            } else {
                lines.extend(report_lines);
            }
            lines
        }
        ToolDetails::Todos { items } => {
            // Reuse the canonical text rendering from `aj-tools`
            // so the interactive view matches the wire content the
            // model sees in `tool_result`. `format_todo_list`
            // sanitises each item's content internally, so the
            // strikethrough SGR it emits for completed items
            // survives but raw control bytes in the content do not.
            let formatted = aj_tools::tools::todo::format_todo_list(items);
            formatted.split('\n').map(|s| s.to_string()).collect()
        }
        ToolDetails::Json(value) => {
            let formatted =
                serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
            formatted.split('\n').map(|s| s.to_string()).collect()
        }
        ToolDetails::Image {
            mime_type,
            original_dimensions: (orig_w, orig_h),
            displayed_dimensions: (disp_w, disp_h),
            ..
        } => {
            // Textual fallback shown until a terminal-image protocol
            // renderer lands. Renders the source dimensions, and the
            // displayed dimensions when a resize occurred.
            let mime_type = sanitize_terminal_output(mime_type);
            let line = if (orig_w, orig_h) == (disp_w, disp_h) {
                format!("[image: {mime_type} · {orig_w}x{orig_h}]")
            } else {
                format!("[image: {mime_type} · {orig_w}x{orig_h} → {disp_w}x{disp_h}]")
            };
            vec![style::dim(&line)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_agent::tool::ToolDetails;
    use aj_tui::ansi::visible_width;

    use crate::config::theme::{Theme, ThemeHandle, chat_theme};

    fn theme() -> ChatTheme {
        chat_theme(&ThemeHandle::new(Theme::bundled_dark()), true)
    }

    /// Build a shared settings handle with the given tool-expansion
    /// mode and inline images enabled (the constructor default the
    /// tests assume). Tests that flip expansion at runtime keep the
    /// returned handle and toggle it.
    fn settings(tools_expanded: bool) -> RenderSettings {
        RenderSettings::new(false, tools_expanded, true)
    }

    fn strip_ansi(s: &str) -> String {
        let mut out: Vec<u8> = Vec::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'm' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).expect("strip_ansi: surviving bytes remain valid UTF-8")
    }

    /// True when the visible content of `line` (after stripping
    /// ANSI escapes) is empty or all-whitespace — i.e. the row is
    /// a bg-painted padding row.
    fn is_blank_row(line: &str) -> bool {
        strip_ansi(line).trim().is_empty()
    }

    #[test]
    fn header_includes_tool_name_and_args_summary() {
        let args = serde_json::json!({"path": "/tmp/foo.txt"});
        let mut c =
            ToolExecutionComponent::new("read_file".to_string(), &args, &theme(), settings(true));
        let lines = c.render(80);
        // First and last rows are bg-painted blank padding.
        assert!(is_blank_row(&lines[0]));
        assert!(is_blank_row(lines.last().expect("non-empty render")));
        // Header lands inside the bubble. After stripping ANSI and
        // trim, it starts with the spinner glyph and includes the
        // tool name plus the args summary.
        let header_plain = strip_ansi(&lines[1]);
        let header_trimmed = header_plain.trim_start();
        assert!(header_trimmed.starts_with("…"), "{header_trimmed:?}");
        assert!(header_trimmed.contains("read_file"));
        assert!(header_trimmed.contains("path=\"/tmp/foo.txt\""));
    }

    #[test]
    fn header_only_renders_just_the_header_without_bubble_or_body() {
        let args = serde_json::json!({"path": "/tmp/foo.txt"});
        let mut c =
            ToolExecutionComponent::new("read_file".to_string(), &args, &theme(), settings(true));
        c.update_result(
            &ToolDetails::Text {
                summary: "/tmp/foo.txt".into(),
                body: "secret body line".into(),
            },
            &[],
            false,
        );
        c.set_header_only(true);
        let lines = c.render(80);
        // No bg-painted blank padding rows: the first row carries the
        // header glyph rather than being blank.
        assert!(!is_blank_row(&lines[0]), "{:?}", lines[0]);
        let first_trimmed = strip_ansi(&lines[0]);
        let first_trimmed = first_trimmed.trim_start();
        assert!(
            first_trimmed.starts_with('✓')
                || first_trimmed.starts_with('…')
                || first_trimmed.starts_with('✗'),
            "{first_trimmed:?}",
        );
        let joined = lines.iter().map(|l| strip_ansi(l)).collect::<String>();
        assert!(joined.contains("read_file"), "{joined:?}");
        assert!(joined.contains("/tmp/foo.txt"), "{joined:?}");
        // The body text must not leak into the header-only render.
        assert!(!joined.contains("secret body line"), "{joined:?}");
    }

    #[test]
    fn toggling_header_only_off_restores_the_bubble() {
        let args = serde_json::json!({"path": "/tmp/foo.txt"});
        let mut c =
            ToolExecutionComponent::new("read_file".to_string(), &args, &theme(), settings(true));
        c.set_header_only(true);
        // Header-only: first row is the header, not a blank pad.
        assert!(!is_blank_row(&c.render(80)[0]));
        // Flipping back restores the bubble's blank top/bottom pads.
        c.set_header_only(false);
        let lines = c.render(80);
        assert!(is_blank_row(&lines[0]));
        assert!(is_blank_row(lines.last().expect("non-empty render")));
        let header_trimmed = strip_ansi(&lines[1]);
        let header_trimmed = header_trimmed.trim_start();
        assert!(header_trimmed.starts_with('…'), "{header_trimmed:?}");
        assert!(header_trimmed.contains("read_file"));
    }

    #[test]
    fn header_only_rows_never_exceed_width() {
        // A long args summary plus a long body must still wrap to fit
        // the render width in the header-only path; nothing should
        // exceed `width`.
        let args = serde_json::json!({
            "command": "echo hi",
            "description": "x".repeat(200),
        });
        let mut c =
            ToolExecutionComponent::new("bash".to_string(), &args, &theme(), settings(true));
        c.update_result(
            &ToolDetails::Text {
                summary: String::new(),
                body: "y".repeat(300),
            },
            &[],
            false,
        );
        c.set_header_only(true);
        let width = 60;
        let lines = c.render(width);
        for (i, line) in lines.iter().enumerate() {
            let w = visible_width(line);
            assert!(
                w <= width,
                "line {i} exceeds width: {w} > {width}: {line:?}"
            );
        }
    }

    #[test]
    fn finalizing_with_text_body_renders_summary_and_body_inside_the_bubble() {
        let mut c = ToolExecutionComponent::new(
            "read_file".to_string(),
            &serde_json::json!({}),
            &theme(),
            settings(true),
        );
        c.update_result(
            &ToolDetails::Text {
                summary: "/tmp/foo.txt".into(),
                body: "line one\nline two".into(),
            },
            &[],
            false,
        );
        let lines: Vec<_> = c.render(80).iter().map(|l| strip_ansi(l)).collect();
        // Success → ✓ glyph in the header row.
        assert!(
            lines
                .iter()
                .any(|l| l.trim_start().starts_with("✓") && l.contains("read_file")),
        );
        // Summary and body lines land somewhere inside the bubble.
        // Stripping ANSI leaves trailing spaces (the bg fills the
        // row to the full terminal width), so a `contains` check
        // against `trim` is the robust shape.
        assert!(
            lines.iter().any(|l| l.trim().contains("/tmp/foo.txt")),
            "{lines:#?}",
        );
        assert!(lines.iter().any(|l| l.trim().contains("line one")));
        assert!(lines.iter().any(|l| l.trim().contains("line two")));
    }

    #[test]
    fn error_status_renders_a_red_cross_in_the_header() {
        let mut c = ToolExecutionComponent::new(
            "bash".to_string(),
            &serde_json::json!({}),
            &theme(),
            settings(true),
        );
        c.update_result(
            &ToolDetails::Text {
                summary: "boom".into(),
                body: String::new(),
            },
            &[],
            true,
        );
        let lines: Vec<_> = c.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(lines.iter().any(|l| l.trim_start().starts_with("✗")));
    }

    #[test]
    fn nonzero_bash_exit_code_paints_as_failure_even_without_is_error() {
        // The bash tool deliberately leaves `is_error: false` on a
        // non-zero exit so the model can read `exit_code` from the
        // structured payload and decide for itself; the *visual*
        // however needs to match what the user sees in the `[exit
        // N]` footer. Verify the bubble paints with the failure
        // glyph in that case.
        let mut c = ToolExecutionComponent::new(
            "bash".to_string(),
            &serde_json::json!({}),
            &theme(),
            settings(true),
        );
        c.update_result(
            &ToolDetails::Bash {
                command: "exit 1".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(1),
                truncated: false,
                full_output_path: None,
                stdout_truncation: None,
                stderr_truncation: None,
                task_id: None,
            },
            &[],
            false,
        );
        let lines: Vec<_> = c.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(
            lines.iter().any(|l| l.trim_start().starts_with("✗")),
            "expected ✗ glyph in header for non-zero exit; got {lines:?}",
        );
    }

    #[test]
    fn zero_bash_exit_code_still_paints_as_success() {
        // Don't regress the happy path: a zero exit must keep the
        // green check even though we now look at exit_code.
        let mut c = ToolExecutionComponent::new(
            "bash".to_string(),
            &serde_json::json!({}),
            &theme(),
            settings(true),
        );
        c.update_result(
            &ToolDetails::Bash {
                command: "echo hi".into(),
                stdout: "hi\n".into(),
                stderr: String::new(),
                exit_code: Some(0),
                truncated: false,
                full_output_path: None,
                stdout_truncation: None,
                stderr_truncation: None,
                task_id: None,
            },
            &[],
            false,
        );
        let lines: Vec<_> = c.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(
            lines.iter().any(|l| l.trim_start().starts_with("✓")),
            "expected ✓ glyph in header for zero exit; got {lines:?}",
        );
    }

    #[test]
    fn missing_bash_exit_code_paints_as_success_when_not_flagged() {
        // Signal-terminated processes (e.g. SIGTERM after a
        // timeout) leave `exit_code: None`. Those are catastrophic
        // and the agent already raises `is_error: true` in that
        // path; with the flag clear we don't second-guess the
        // tool and keep the success styling.
        let mut c = ToolExecutionComponent::new(
            "bash".to_string(),
            &serde_json::json!({}),
            &theme(),
            settings(true),
        );
        c.update_result(
            &ToolDetails::Bash {
                command: "true".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                truncated: false,
                full_output_path: None,
                stdout_truncation: None,
                stderr_truncation: None,
                task_id: None,
            },
            &[],
            false,
        );
        let lines: Vec<_> = c.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(
            lines.iter().any(|l| l.trim_start().starts_with("✓")),
            "expected ✓ glyph in header when exit_code is None and is_error is false; got {lines:?}",
        );
    }

    #[test]
    fn long_string_args_get_truncated_with_an_ellipsis() {
        let long = "x".repeat(200);
        let s = format_args(&serde_json::Value::String(long.clone()));
        // The summary is wrapped in quotes; the inner body should be
        // capped well before the input length.
        assert!(s.starts_with('"'));
        assert!(s.contains('…'));
        assert!(s.len() < long.len());
    }

    #[test]
    fn body_lines_wider_than_width_get_wrapped_to_fit() {
        // Regression: clippy / build output regularly contains
        // single lines wider than the terminal (e.g. `help: try:
        // ...` suggestions naming a fully-qualified type). The
        // component must wrap them so the strict line-width check
        // in `Tui::render` doesn't panic on the next frame.
        let mut c = ToolExecutionComponent::new(
            "bash".to_string(),
            &serde_json::json!({}),
            &theme(),
            settings(true),
        );
        let long_line = "x".repeat(300);
        c.update_result(
            &ToolDetails::Text {
                summary: String::new(),
                body: long_line.clone(),
            },
            &[],
            false,
        );
        let width = 80;
        let lines = c.render(width);
        for (i, line) in lines.iter().enumerate() {
            let w = visible_width(line);
            assert!(
                w <= width,
                "line {i} exceeds width: {w} > {width}: {line:?}",
            );
        }
    }

    #[test]
    fn header_wider_than_width_gets_wrapped_to_fit() {
        // A long `description=` field can push the header past the
        // terminal edge on its own. Make sure the wrap path covers
        // the header line too.
        let args = serde_json::json!({
            "command": "echo hi",
            "description": "x".repeat(200),
        });
        let mut c =
            ToolExecutionComponent::new("bash".to_string(), &args, &theme(), settings(true));
        let width = 60;
        let lines = c.render(width);
        for (i, line) in lines.iter().enumerate() {
            let w = visible_width(line);
            assert!(
                w <= width,
                "line {i} exceeds width: {w} > {width}: {line:?}",
            );
        }
    }

    /// `ToolDetails::Bash` payload with the given task id and
    /// stdout; the other fields stay at their background-launch
    /// defaults.
    fn bash_task_details(stdout: &str, task_id: Option<usize>) -> ToolDetails {
        ToolDetails::Bash {
            command: "sleep 5".into(),
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: None,
            truncated: false,
            full_output_path: None,
            stdout_truncation: None,
            stderr_truncation: None,
            task_id,
        }
    }

    #[test]
    fn bash_task_id_renders_a_header_badge() {
        // The badge marks the cell as a background launch. It must
        // render off the persisted `ToolDetails::Bash.task_id`
        // alone (the resume path has no task events and no
        // registry), so a bare `update_result` is the whole setup.
        let mut c = ToolExecutionComponent::new(
            "bash".to_string(),
            &serde_json::json!({}),
            &theme(),
            settings(true),
        );
        c.update_result(&bash_task_details("", Some(3)), &[], false);
        let joined = c
            .render(80)
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("[task #3]"), "{joined:?}");

        // Foreground calls (no task id) carry no badge.
        c.update_result(&bash_task_details("", None), &[], false);
        let joined = c
            .render(80)
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.contains("[task #"), "{joined:?}");
    }

    #[test]
    fn task_cell_live_tails_after_result_then_freezes_on_finish() {
        let mut c = ToolExecutionComponent::new(
            "bash".to_string(),
            &serde_json::json!({}),
            &theme(),
            settings(true),
        );
        // Launch result (the "started" snapshot) finalizes the call,
        // but a background cell keeps accepting partial updates.
        c.update_result(&bash_task_details("", Some(3)), &[], false);
        c.update_partial(&bash_task_details("LIVETAIL", Some(3)), &[]);
        let joined = c
            .render(80)
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("LIVETAIL"), "{joined:?}");

        // The terminal status freezes the cell: badge gains the
        // outcome, the glyph follows it, and stragglers are ignored.
        c.finish_task(TaskStatus::Exited(Some(0)));
        let joined = c
            .render(80)
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("[task #3 · exited 0]"), "{joined:?}");
        assert!(
            joined.lines().any(|l| l.trim_start().starts_with('✓')),
            "{joined:?}"
        );
        c.update_partial(&bash_task_details("AFTERFREEZE", Some(3)), &[]);
        let joined = c
            .render(80)
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.contains("AFTERFREEZE"), "{joined:?}");
        assert!(joined.contains("LIVETAIL"), "{joined:?}");
    }

    #[test]
    fn task_failure_statuses_paint_the_failed_glyph() {
        for (status, badge) in [
            (TaskStatus::Killed, "[task #4 · killed]"),
            (TaskStatus::Exited(Some(2)), "[task #4 · exited 2]"),
            (TaskStatus::Exited(None), "[task #4 · terminated by signal]"),
        ] {
            let mut c = ToolExecutionComponent::new(
                "bash".to_string(),
                &serde_json::json!({}),
                &theme(),
                settings(true),
            );
            c.update_result(&bash_task_details("", Some(4)), &[], false);
            c.finish_task(status);
            let joined = c
                .render(80)
                .iter()
                .map(|l| strip_ansi(l))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(joined.contains(badge), "{status:?}: {joined:?}");
            assert!(
                joined.lines().any(|l| l.trim_start().starts_with('✗')),
                "{status:?}: {joined:?}"
            );
        }
    }

    #[test]
    fn pending_succeeded_failed_share_one_tool_background() {
        // The bundled themes intentionally point all three
        // `toolPendingBg` / `toolSuccessBg` / `toolErrorBg` color
        // tokens at a single `toolBg` palette var — status is
        // communicated through the header glyph (… / ✓ / ✗) and
        // not the bubble tint. Grab the bg-paint prefix off the
        // first row of each render and verify they're identical.
        let mut c = ToolExecutionComponent::new(
            "bash".to_string(),
            &serde_json::json!({}),
            &theme(),
            settings(true),
        );

        let bg_sample = |c: &mut ToolExecutionComponent| -> String {
            let lines = c.render(40);
            // The first row is the top bg-pad — entirely a bg
            // escape + spaces + bg-close. Take everything up to
            // the first space; that's our prefix.
            let first = &lines[0];
            let cut = first.find(' ').unwrap_or(first.len());
            first[..cut].to_string()
        };

        let pending = bg_sample(&mut c);
        c.update_result(
            &ToolDetails::Text {
                summary: String::new(),
                body: "ok".into(),
            },
            &[],
            false,
        );
        let succeeded = bg_sample(&mut c);
        c.update_result(
            &ToolDetails::Text {
                summary: String::new(),
                body: "boom".into(),
            },
            &[],
            true,
        );
        let failed = bg_sample(&mut c);

        assert_eq!(
            pending, succeeded,
            "pending and succeeded should share the unified tool bg"
        );
        assert_eq!(
            pending, failed,
            "pending and failed should share the unified tool bg"
        );
    }

    #[test]
    fn repeated_renders_return_byte_identical_output() {
        // The whole point of the refactor: a finalised tool with a
        // multi-line body should render the same bytes on every
        // subsequent frame, *and* the second frame must not have
        // to re-do the wrap pass. We can't directly observe cache
        // hits from outside, but byte-equality on the rendered
        // output is the user-visible contract and is what the
        // diff-aware terminal write depends on. A regression that
        // produced subtly different ANSI on cache hit (e.g. a
        // closure that captured stateful counters) would surface
        // here.
        let body = (0..50)
            .map(|i| format!("line {i:02}: lorem ipsum dolor sit amet"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut c = ToolExecutionComponent::new(
            "read_file".to_string(),
            &serde_json::json!({}),
            &theme(),
            settings(true),
        );
        c.update_result(
            &ToolDetails::Text {
                summary: "/tmp/big.txt".into(),
                body,
            },
            &[],
            false,
        );
        let first = c.render(80);
        let second = c.render(80);
        let third = c.render(80);
        assert_eq!(first, second);
        assert_eq!(second, third);
    }

    #[test]
    fn body_with_carriage_returns_and_erase_in_line_renders_flush_width() {
        // Regression for the ragged-right-edge bug. Real tool output
        // (cargo / git / pip progress, ANSI-coloured clippy output)
        // ships `\r` overprints and `ESC[K` erase-in-line sequences
        // that disagree with the renderer's width measurement: `\r`
        // was counted as zero-width but the terminal honours it, and
        // `ESC[K` was stripped from measurement but its terminal side
        // effect erases trailing cells with the *default* background
        // -- chewing a hole in the bubble's painted tint. The body
        // sanitisation in `render_details_body` removes both before
        // they reach the wrap path, so every row of the bubble lands
        // at exactly the render width.
        let mut c = ToolExecutionComponent::new(
            "bash".to_string(),
            &serde_json::json!({}),
            &theme(),
            settings(true),
        );
        // Progress-style overprint, an SGR-reset followed by an
        // erase-in-line (the canonical "ragged right edge"
        // pattern), and a control byte (`\x08`) for good measure.
        let body =
            "progress: 10%\rprogress: 20%\nstatus\x1b[0m\x1b[Kdone\nback\x08space\n".to_string();
        c.update_result(
            &ToolDetails::Text {
                summary: String::new(),
                body,
            },
            &[],
            false,
        );
        let width = 60;
        let lines = c.render(width);
        for (i, line) in lines.iter().enumerate() {
            let w = visible_width(line);
            assert!(
                w == width,
                "row {i} is not flush to width: visible_width={w}, expected={width}: {line:?}",
            );
            assert!(!line.contains('\r'), "row {i} still carries CR: {line:?}");
            // Erase-in-line and other non-styling CSI must not survive.
            assert!(
                !line.contains("\x1b[K"),
                "row {i} still carries erase-in-line: {line:?}",
            );
            // Backspace and other C0 controls must not survive.
            assert!(
                !line.chars().any(|c| {
                    let code = u32::from(c);
                    code <= 0x1f && c != '\t' && c != '\n' && c != '\x1b'
                }),
                "row {i} still carries a non-tab/-newline control byte: {line:?}",
            );
        }
    }

    #[test]
    fn bash_command_field_strips_control_bytes_from_header() {
        // Defence in depth: if a future caller stuffs an ANSI / CR /
        // control byte into `ToolDetails::Bash.command`, the dim-
        // styled `$ <command>` header line must still render flush
        // to width and contain only the visible command text.
        let mut c = ToolExecutionComponent::new(
            "bash".to_string(),
            &serde_json::json!({}),
            &theme(),
            settings(true),
        );
        c.update_result(
            &ToolDetails::Bash {
                command: "echo \x1b[31mboom\x1b[0m\rmore".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                truncated: false,
                full_output_path: None,
                stdout_truncation: None,
                stderr_truncation: None,
                task_id: None,
            },
            &[],
            false,
        );
        let width = 60;
        let lines = c.render(width);
        for (i, line) in lines.iter().enumerate() {
            let w = visible_width(line);
            assert!(
                w == width,
                "row {i} not flush to width: visible_width={w}: {line:?}",
            );
            assert!(!line.contains('\r'), "row {i} still carries CR: {line:?}");
        }
        // The visible command text comes through, sans ANSI / CR.
        let plain: Vec<_> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(
            plain.iter().any(|l| l.contains("echo boommore")),
            "{plain:#?}",
        );
    }

    /// Make sure the global keybinding registry is populated for
    /// tests that rely on the `aj.tools.expand` lookup inside
    /// `expand_hint`. Idempotent — installing repeatedly just
    /// re-points the global at an equivalent manager.
    fn install_default_keybindings() {
        crate::config::keybindings::install_global_manager_defaults();
    }

    #[test]
    fn text_body_collapses_to_ten_lines_with_alt_o_hint() {
        install_default_keybindings();
        let body = (1..=30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let s = settings(false);
        let mut c = ToolExecutionComponent::new(
            "read_file".to_string(),
            &serde_json::json!({}),
            &theme(),
            s.clone(),
        );
        c.update_result(
            &ToolDetails::Text {
                summary: String::new(),
                body: body.clone(),
            },
            &[],
            false,
        );
        // First ten lines are visible plus a hint line.
        let plain: Vec<String> = c.body.iter().map(|l| strip_ansi(l)).collect();
        for i in 1..=10 {
            assert!(
                plain.iter().any(|l| l == &format!("line {i}")),
                "missing line {i}: {plain:#?}",
            );
        }
        assert!(
            !plain.iter().any(|l| l == "line 11"),
            "line 11 should be hidden in collapsed mode: {plain:#?}",
        );
        let hint = plain
            .iter()
            .find(|l| l.contains("more lines"))
            .expect("hint present");
        assert!(hint.contains("Alt+O"), "hint missing key: {hint:?}");
        assert!(hint.contains("20"), "hint missing count: {hint:?}");

        // Expand via the shared settings → the body cache is
        // rebuilt at the next render, exposing all 30 lines with no
        // hint. (Production toggles invalidate + repaint; the render
        // call here stands in for that repaint.)
        s.set_tools_expanded(true);
        c.render(80);
        let plain: Vec<String> = c.body.iter().map(|l| strip_ansi(l)).collect();
        for i in 1..=30 {
            assert!(
                plain.iter().any(|l| l == &format!("line {i}")),
                "expanded: missing line {i}: {plain:#?}",
            );
        }
        assert!(
            !plain.iter().any(|l| l.contains("more lines")),
            "hint should be gone after expand: {plain:#?}",
        );
    }

    #[test]
    fn bash_stdout_tail_compacts_to_five_lines_with_earlier_hint() {
        install_default_keybindings();
        let stdout = (1..=20)
            .map(|i| format!("out {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut c = ToolExecutionComponent::new(
            "bash".to_string(),
            &serde_json::json!({}),
            &theme(),
            settings(false),
        );
        c.update_result(
            &ToolDetails::Bash {
                command: "seq 20".into(),
                stdout: stdout.clone(),
                stderr: String::new(),
                exit_code: Some(0),
                truncated: false,
                full_output_path: None,
                stdout_truncation: None,
                stderr_truncation: None,
                task_id: None,
            },
            &[],
            false,
        );
        let plain: Vec<String> = c.body.iter().map(|l| strip_ansi(l)).collect();
        // Last five lines visible.
        for i in 16..=20 {
            assert!(
                plain.iter().any(|l| l == &format!("out {i}")),
                "missing tail line {i}: {plain:#?}",
            );
        }
        // Earlier lines hidden.
        assert!(
            !plain.iter().any(|l| l == "out 15"),
            "out 15 should be hidden: {plain:#?}",
        );
        let hint = plain
            .iter()
            .find(|l| l.contains("earlier lines"))
            .expect("hint present");
        assert!(hint.contains("Alt+O"), "hint missing key: {hint:?}");
        assert!(hint.contains("15"), "hint missing count: {hint:?}");
    }

    #[test]
    fn expand_hint_drops_key_when_no_binding_is_registered() {
        // Test the pure formatter directly so we don't have to
        // mutate the process-wide keybindings manager (which is
        // shared with sibling tests running in parallel).
        assert_eq!(
            format_expand_hint(7, HintKind::More, None),
            "… (7 more lines)",
        );
        assert_eq!(
            format_expand_hint(3, HintKind::Earlier, None),
            "… (3 earlier lines)",
        );
        assert_eq!(
            format_expand_hint(2, HintKind::More, Some("Alt+O")),
            "… (2 more lines, Alt+O to expand)",
        );
    }
}
