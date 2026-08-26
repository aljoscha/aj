//! The sub-agent box: the parent-transcript widget for one sub-agent
//! run.
//!
//! A gray box with a one-line `{glyph} agent {N} · {task}` title and a body,
//! separated by a blank row. Once the sub-agent is done the body is its
//! report, rendered as markdown the same way assistant prose renders in the
//! transcript, and folded to a head preview when collapsed the same way tool
//! cells fold long output (the shared tools-expand toggle shows the whole
//! report). While it runs the glyph is a wall-clock spinner frame and the
//! body is a single latest-activity line that clips with an ellipsis.
//!
//! The running glyph animates on a wall-clock, matching the status loader's
//! cadence, so it keeps spinning between sub-agent events rather than
//! freezing (a frozen glyph reads as stalled). Two pieces outside this module
//! drive that: the status loader arms its redraw tick while any sub-agent
//! runs, not only while the viewed agent is busy (see
//! [`StatusState::animating`](crate::status::StatusState::animating)), and a
//! `Running` box bypasses the transcript render cache so each redraw rebuilds
//! it with a fresh frame.
//!
//! The box renders from box metadata alone: it never composites the
//! child transcript or tail-windows it, so it needs no access to the
//! sub's transcript. That is what lets a resumed sub-agent's transcript
//! stay unmaterialized until observed. The full conversation is one
//! Observe away, which swaps the whole list over to that agent's
//! transcript ([`set_active_view`](aj_app::chat::ChatState::set_active_view)).
//!
//! Like the other transcript widgets, the box is built fresh per draw
//! from data extracted out of [`ChatState`](aj_app::chat::ChatState) at
//! build time. Drawing needs no model access, so the `ListView`
//! builder's shared borrow never nests or escapes.

use std::time::Duration;

use aj_app::chat::{SubAgentEntry, SubAgentStatus};
use aj_app::markdown::{Emphasis, RenderOpts};
use aj_tools::sanitize_terminal_output;
use vaxis::cell::{Cell, Color, Style};
use vaxis::vxfw::{
    DrawContext, MaxSize, Overflow, RichText, Size, SubSurface, Surface, TextSpan, Widget,
};

use crate::bubble::{MIN_BUBBLE_WIDTH, PADDING_X, PADDING_Y};
use crate::markdown_view::{MarkdownSegment, MarkdownStyles, draw_markdown_segments};
use crate::tool_cell::{HintKind, REPORT_COLLAPSED_LINES, expand_hint};
use crate::transcript::TranscriptStyles;

/// Spinner frames for a `Running` box's glyph. A local copy of the
/// status-loader frame set: the modules stay decoupled, so the box
/// does not reach into `status.rs`'s private const.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Milliseconds each spinner frame shows for. Matches the status loader's
/// cadence so the box glyph and the main loader advance at the same rate. A
/// local const keeps the box decoupled from `status.rs`.
const SPINNER_INTERVAL_MS: u128 = 80;

/// The spinner glyph for a `Running` box `elapsed` into its current run.
/// Derived from the wall-clock, like the status loader, so the animation runs
/// at a steady cadence independent of when the box happens to be rebuilt.
fn spinner_frame(elapsed: Duration) -> &'static str {
    // Modulo the frame count so the index is bounded and fits usize.
    let n = u128::try_from(SPINNER_FRAMES.len()).unwrap_or(u128::MAX);
    let idx = usize::try_from((elapsed.as_millis() / SPINNER_INTERVAL_MS) % n).unwrap_or(0);
    SPINNER_FRAMES[idx]
}

/// On-screen representation of one sub-agent run, boxed inside the
/// parent's transcript.
pub(crate) struct SubAgentBox {
    /// The `{glyph} agent {N} · {task}` title spans, one logical line
    /// (the task text is whitespace-normalized at build time). The
    /// glyph is a check when done or a spinner frame while running.
    title: Vec<TextSpan>,
    /// The content below the title, separated from it by a blank row.
    body: BoxBody,
    /// The box tint (the shared pending-tool gray, matching `aj`).
    bg: Color,
}

/// The box body below the title.
enum BoxBody {
    /// Nothing below the header: a title-only box (a running sub with no
    /// activity yet, or a concluded sub with an empty report).
    Empty,
    /// A running box's latest-activity line. Clips to one row with an
    /// ellipsis, no soft wrap.
    Activity(Vec<TextSpan>),
    /// A concluded box's report, rendered as markdown (folded to a head
    /// preview when collapsed), with an optional dim fold hint below it. The
    /// markdown is laid out at draw time because it needs the box width.
    Report {
        segments: Vec<MarkdownSegment>,
        markdown: MarkdownStyles,
        hint: Option<TextSpan>,
    },
}

/// Build the box for `entry` from its metadata: the report (done) or a
/// spinner glyph plus latest-activity line (running). Reads no
/// transcript.
///
/// `expanded` is the session-wide tools-expand flag: when set, a long
/// done-report renders in full, otherwise it folds to a head preview.
/// `syntax_highlight` is the session-wide flag threaded into the report's
/// markdown segments, the same one assistant prose uses.
pub(crate) fn build_subagent_box(
    entry: &SubAgentEntry,
    expanded: bool,
    syntax_highlight: bool,
    styles: &TranscriptStyles,
) -> SubAgentBox {
    let span = |text: String, style: Style| TextSpan {
        text,
        style,
        ..TextSpan::default()
    };
    let glyph = match entry.status {
        // The running glyph is a wall-clock spinner frame, kept dim (the muted
        // gray the box uses for its metadata). See the module docs for the
        // redraw pump and cache bypass that keep it animating.
        SubAgentStatus::Running => {
            span(spinner_frame(entry.started_at.elapsed()).into(), styles.dim)
        }
        SubAgentStatus::Done => span("✓".into(), styles.success),
        // A truncated run finished but its report is partial, and a failed
        // run errored: distinct glyphs and tints so the box reads its
        // outcome at a glance.
        SubAgentStatus::Truncated => span("⚠".into(), styles.warning),
        SubAgentStatus::Failed => span("✗".into(), styles.error),
    };
    // Collapse the task text to one line so the title never wraps.
    // Over-wide titles are truncated at draw time (the draw disables
    // soft wrapping, so the wrap engine's ellipsis overflow applies).
    let task = entry.task.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = vec![
        glyph,
        span(" ".into(), styles.text),
        span(format!("agent {}", entry.child), styles.bold),
        span(" · ".into(), styles.text),
        span(task, styles.dim),
    ];
    // The body is metadata only. A running box shows its latest-activity
    // line; a concluded box (done, truncated, or failed) shows its
    // report, folded. An empty report is a real, accepted case (a
    // sub-agent that concluded on a tool call), and renders a thin
    // title-only box.
    let body = match entry.status {
        SubAgentStatus::Running => match entry.latest_activity.as_deref() {
            Some(activity) if !activity.is_empty() => {
                BoxBody::Activity(vec![span(activity.into(), styles.dim)])
            }
            _ => BoxBody::Empty,
        },
        SubAgentStatus::Done | SubAgentStatus::Truncated | SubAgentStatus::Failed => {
            match entry.report.as_deref() {
                Some(report) if !report.is_empty() => {
                    report_body(report, expanded, syntax_highlight, styles)
                }
                _ => BoxBody::Empty,
            }
        }
    };
    SubAgentBox {
        title,
        body,
        bg: styles.tool_pending_bg,
    }
}

/// Build the concluded-report body. Collapsed, the report folds to its first
/// [`REPORT_COLLAPSED_LINES`] source lines plus a `… (N more lines, <key> to
/// expand)` hint, matching how tool cells fold long output. Expanded shows the
/// whole report. The retained source renders as markdown, so the report reads
/// like assistant prose in the transcript.
fn report_body(
    report: &str,
    expanded: bool,
    syntax_highlight: bool,
    styles: &TranscriptStyles,
) -> BoxBody {
    // Sanitize before splitting so control bytes and escapes leave both the
    // rendered content and the source-line count, matching how tool cells
    // process their bodies. A bare carriage return would otherwise underflow
    // the wrap engine at draw time.
    let report = sanitize_terminal_output(report);
    let mut lines: Vec<&str> = report.split('\n').collect();
    // Drop a trailing empty line from a report that ended in `\n`: the
    // box's bottom pad already separates it from the next entry.
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    // Fold by source line, before markdown rendering, so the hidden count and
    // the fold match how tool output folds. Markdown then reflows the retained
    // lines at draw width.
    let hidden = if expanded {
        0
    } else {
        lines.len().saturating_sub(REPORT_COLLAPSED_LINES)
    };
    if hidden > 0 {
        lines.truncate(REPORT_COLLAPSED_LINES);
    }
    let segment = MarkdownSegment {
        text: lines.join("\n"),
        opts: RenderOpts {
            hyperlinks: styles.hyperlinks,
            default_emphasis: Emphasis::default(),
            syntax_highlight,
        },
        base_style: styles.text,
    };
    let hint = (hidden > 0).then(|| TextSpan {
        text: expand_hint(hidden, HintKind::More),
        style: styles.dim,
        ..TextSpan::default()
    });
    BoxBody::Report {
        segments: vec![segment],
        markdown: styles.markdown,
        hint,
    }
}

/// Composite a surface tree (buffer plus children by z-order) into a
/// flat row-major cell grid, the way `Surface::render` paints it.
/// Out-of-bounds child cells are clipped.
pub(crate) fn surface_rows(surface: &Surface) -> Vec<Vec<Cell>> {
    let w = usize::from(surface.size.width);
    let h = usize::from(surface.size.height);
    let mut grid = vec![vec![Cell::default(); w]; h];
    for (i, cell) in surface.buffer.iter().enumerate() {
        grid[i / w][i % w] = cell.clone();
    }
    let mut order: Vec<&SubSurface> = surface.children.iter().collect();
    order.sort_by_key(|c| c.z_index);
    for child in order {
        let sub = surface_rows(&child.surface);
        for (r, sub_row) in sub.iter().enumerate() {
            let Ok(rr) = usize::try_from(child.origin.row + i32::try_from(r).expect("row fits"))
            else {
                continue;
            };
            if rr >= h {
                continue;
            }
            for (c, cell) in sub_row.iter().enumerate() {
                let Ok(cc) =
                    usize::try_from(child.origin.col + i32::try_from(c).expect("col fits"))
                else {
                    continue;
                };
                if cc >= w {
                    continue;
                }
                grid[rr][cc] = cell.clone();
            }
        }
    }
    grid
}

impl SubAgentBox {
    /// Draw the body at `inner_ctx`'s width. A running activity line stays on
    /// one row and clips with an ellipsis; a concluded report renders as
    /// pre-wrapped markdown rows plus its optional fold hint. Empty body draws
    /// no rows.
    fn body_rows(&self, inner_ctx: &DrawContext) -> Vec<Vec<Cell>> {
        match &self.body {
            BoxBody::Empty => Vec::new(),
            BoxBody::Activity(spans) => {
                let mut text = RichText::new(spans.clone());
                text.softwrap = false;
                text.overflow = Overflow::Ellipsis;
                surface_rows(&text.draw(inner_ctx))
            }
            BoxBody::Report {
                segments,
                markdown,
                hint,
            } => {
                let width = inner_ctx.max.width.unwrap_or(inner_ctx.min.width);
                let mut rows = surface_rows(&draw_markdown_segments(
                    inner_ctx, segments, markdown, width,
                ));
                // The fold hint is a plain dim line below the report, clipped
                // to one row like the activity line.
                if let Some(hint) = hint {
                    let mut text = RichText::new(vec![hint.clone()]);
                    text.softwrap = false;
                    text.overflow = Overflow::Ellipsis;
                    rows.extend(surface_rows(&text.draw(inner_ctx)));
                }
                rows
            }
        }
    }

    /// Draw the title spans as a single ellipsis-truncated row at the
    /// inner width.
    fn title_rows(&self, inner_ctx: &DrawContext) -> Vec<Vec<Cell>> {
        let mut title = RichText::new(self.title.clone());
        // No soft wrap: the one logical line clips to the width with
        // an `…`, matching `aj`'s whole-title truncation.
        title.softwrap = false;
        title.overflow = Overflow::Ellipsis;
        surface_rows(&title.draw(inner_ctx))
    }

    /// Emit `rows` as a flat surface at origin, no bubble frame. The
    /// degenerate-width fallback, mirroring the tool bubble's.
    fn draw_plain(rows: Vec<Vec<Cell>>, width: u16) -> Surface {
        let height = u16::try_from(rows.len()).expect("box rows fit u16") + 1;
        let mut surface = Surface::with_size(Size { width, height });
        for (r, row) in rows.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                surface.write_cell(
                    u16::try_from(c).expect("col fits"),
                    u16::try_from(r).expect("row fits"),
                    cell.clone(),
                );
            }
        }
        surface
    }
}

impl Widget for SubAgentBox {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let width = ctx.max.width.unwrap_or(ctx.min.width);
        let bubble = width >= MIN_BUBBLE_WIDTH;
        let inner_width = if bubble {
            width - 2 * PADDING_X
        } else {
            width.max(1)
        };
        let inner_ctx = ctx.with_constraints(
            Size {
                width: 0,
                height: 0,
            },
            MaxSize {
                width: Some(inner_width),
                height: None,
            },
        );

        let mut content = self.title_rows(&inner_ctx);
        let body = self.body_rows(&inner_ctx);
        if !body.is_empty() {
            // A blank row separates the header from the body, so the activity
            // line or report doesn't sit flush under the title. An empty
            // `Vec<Cell>` row renders as a tinted blank line (the bubble tint
            // is pre-painted and untouched `default` cells are skipped below).
            content.push(Vec::new());
            content.extend(body);
        }

        if !bubble {
            return Self::draw_plain(content, width);
        }

        // The bubble frame: bg-filled padding around the content rows
        // plus one untinted spacer row, the same rhythm as `Bubble`.
        // Content (title rows followed by body rows) is written directly
        // into the buffer rather than as child surfaces so we can paint
        // the box tint under each composited row.
        let content_height = u16::try_from(content.len()).expect("box rows fit u16");
        let bubble_height = content_height + 2 * PADDING_Y;
        let mut surface = Surface::with_size(Size {
            width,
            height: bubble_height + 1,
        });
        let bg_cell = Cell {
            style: Style {
                bg: self.bg,
                ..Style::default()
            },
            ..Cell::default()
        };
        for row in 0..bubble_height {
            for col in 0..width {
                surface.write_cell(col, row, bg_cell.clone());
            }
        }
        for (r, row) in content.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                // Untouched (`default`) cells are transparent here: skip them
                // so the box tint we already wrote into this row stays, rather
                // than overwriting it with a default-background hole. Writing
                // one and re-tinting it in place would leave a `default`-flagged
                // cell carrying our tint. The diff's default fast-path then
                // treats it as blank and never repaints it, stranding stale
                // gray when the box later shrinks or moves.
                if cell.default {
                    continue;
                }
                // Painted content is already non-default, so tinting its
                // background can't violate that invariant. Cells that carry
                // their own bg (an inner user bubble) keep it, matching how the
                // box paints its background under already-styled inner rows.
                let mut cell = cell.clone();
                if cell.style.bg == Color::Default {
                    cell.style.bg = self.bg;
                }
                surface.write_cell(
                    u16::try_from(c).expect("col fits") + PADDING_X,
                    u16::try_from(r).expect("row fits") + PADDING_Y,
                    cell,
                );
            }
        }
        surface
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_agent::events::{AgentEvent, AgentId, AgentSettings, SubAgentConclusion};
    use aj_agent::message::AgentMessage;
    use aj_agent::tool::ToolDetails;
    use aj_app::chat::{ChatState, EntryKind, reduce};
    use aj_app::session::AgentLifecycle;
    use aj_app::theme::{ColorMode, Theme};
    use aj_models::types::{
        AssistantContent, AssistantMessage, Message, StopReason, TextContent, UserMessage,
    };
    use vaxis::vxfw::MaxSize;

    use super::*;
    use crate::test_support::{draw_ctx, flatten, rows};
    use crate::transcript::TranscriptStyles;

    fn styles() -> TranscriptStyles {
        TranscriptStyles::from_theme(
            &Theme::bundled_dark_with_mode(ColorMode::Truecolor),
            crate::terminal::TerminalCaps::default(),
        )
    }

    fn chat() -> ChatState {
        ChatState::new(
            AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                thinking_display: "default".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            0,
            Arc::new(Vec::new()),
        )
    }

    fn assistant(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            api: "scripted".into(),
            provider: "scripted".into(),
            model: "scripted".into(),
            account: None,
            response_id: None,
            usage: Default::default(),
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        }
    }

    /// Reduce a lifelike sub-agent event sequence into `chat`: spawn,
    /// task prompt, an assistant line, and one bash tool call. The box
    /// stays `Running`, with `bash` as its latest activity (the tool
    /// start is the last live event). The child transcript is built too,
    /// which the metadata box ignores.
    fn reduce_sub_run(chat: &mut ChatState, life: &mut AgentLifecycle) {
        let sub = AgentId::Sub(0);
        let events = vec![
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: sub,
                task: "check the build setup".into(),
                background: false,
                settings: AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "off".into(),
                    thinking_display: "default".into(),
                    speed: "standard".into(),
                    verbosity: "default".into(),
                },
            },
            AgentEvent::AgentStart { agent_id: sub },
            AgentEvent::MessageEnd {
                agent_id: sub,
                message: AgentMessage::wire(Message::User(UserMessage::text(
                    "check the build setup",
                ))),
            },
            AgentEvent::MessageEnd {
                agent_id: sub,
                message: AgentMessage::wire(Message::Assistant(assistant("On it."))),
            },
            AgentEvent::ToolExecutionStart {
                agent_id: sub,
                call_id: "tu-sub-1".into(),
                tool: "bash".into(),
                args: serde_json::json!({"command": "echo hi"}),
            },
            AgentEvent::ToolExecutionEnd {
                agent_id: sub,
                call_id: "tu-sub-1".into(),
                tool: "bash".into(),
                result: ToolDetails::Bash {
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
                content: Vec::new().into(),
                is_error: false,
            },
        ];
        for event in events {
            let _ = reduce(chat, life, event, None);
        }
    }

    /// Finish the sub run: report lands, run ends.
    fn reduce_sub_end(chat: &mut ChatState, life: &mut AgentLifecycle) {
        let sub = AgentId::Sub(0);
        let events = vec![
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: sub,
                report: "all good".into(),
                conclusion: aj_agent::events::SubAgentConclusion::Completed,
            },
            AgentEvent::AgentEnd {
                agent_id: sub,
                messages: Vec::new(),
            },
        ];
        for event in events {
            let _ = reduce(chat, life, event, None);
        }
    }

    /// Run the sub to completion with `report` as its final report, leaving a
    /// `Done` box in Main's transcript.
    fn finish_with_report(chat: &mut ChatState, life: &mut AgentLifecycle, report: &str) {
        reduce_sub_run(chat, life);
        for event in [
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(0),
                report: report.into(),
                conclusion: aj_agent::events::SubAgentConclusion::Completed,
            },
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(0),
                messages: Vec::new(),
            },
        ] {
            let _ = reduce(chat, life, event, None);
        }
    }

    /// The `SubAgentEntry` of the box in Main's transcript.
    fn box_entry(chat: &ChatState) -> &SubAgentEntry {
        chat.transcript(AgentId::Main)
            .expect("main transcript")
            .entries()
            .iter()
            .find_map(|e| match &e.kind {
                EntryKind::SubAgent(s) => Some(s),
                _ => None,
            })
            .expect("sub-agent box entry")
    }

    fn draw_box_with(chat: &ChatState, width: u16, expanded: bool) -> Surface {
        let s = styles();
        let mut b = build_subagent_box(box_entry(chat), expanded, false, &s);
        b.draw(&draw_ctx(width, None))
    }

    fn draw_box(chat: &ChatState, width: u16) -> Surface {
        draw_box_with(chat, width, false)
    }

    #[test]
    fn spinner_frame_advances_with_elapsed_time() {
        // Frame index is `elapsed / interval % frames`, like the status loader.
        let ms = |n: u64| Duration::from_millis(n);
        assert_eq!(spinner_frame(Duration::ZERO), SPINNER_FRAMES[0]);
        assert_eq!(spinner_frame(ms(80)), SPINNER_FRAMES[1]);
        assert_eq!(spinner_frame(ms(80 * 3 + 10)), SPINNER_FRAMES[3]);
        // Wraps around the frame set.
        assert_eq!(spinner_frame(ms(80 * 10)), SPINNER_FRAMES[0]);
    }

    /// The box spinner shares the status loader's cadence (the spec wants them
    /// to advance at the same rate). The frame sets are deliberate local
    /// copies, but the interval must not silently diverge.
    #[test]
    fn spinner_cadence_matches_the_status_loader() {
        assert_eq!(
            SPINNER_INTERVAL_MS,
            u128::from(crate::status::FRAME_INTERVAL_MS),
        );
    }

    #[test]
    fn compact_box_renders_title_report_and_tint() {
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        reduce_sub_run(&mut chat, &mut life);
        reduce_sub_end(&mut chat, &mut life);
        let s = styles();
        let surface = draw_box(&chat, 60);
        let r = rows(&surface);

        // Frame: bg-painted blank pads around the content, untinted
        // spacer at the bottom. A blank row separates the header from the body.
        assert_eq!(r[0], "", "top pad row is blank");
        assert_eq!(r[1], " ✓ agent 0 · check the build setup");
        assert_eq!(r[2], "", "blank row separates header and body");
        assert_eq!(r[3], " all good", "the report is the body");
        assert_eq!(r.last().unwrap(), "", "spacer row is blank");

        // Tint: every bubble row is the tool-pending gray, the trailing
        // spacer row is untinted.
        let grid = flatten(&surface);
        let h = grid.len();
        for (row_idx, row) in grid.iter().enumerate().take(h - 1) {
            for (col_idx, cell) in row.iter().enumerate() {
                assert_eq!(
                    cell.style.bg, s.tool_pending_bg,
                    "cell ({row_idx},{col_idx}) untinted",
                );
            }
        }
        assert!(grid[h - 1].iter().all(|c| c.style.bg == Color::Default));
    }

    #[test]
    fn box_never_tints_a_default_cell() {
        // A tinted cell must not stay flagged `default`. The render diff's
        // default fast-path treats two `default` cells as equal regardless of
        // background, so a `default` cell carrying the box tint reads as blank
        // and is never repainted, stranding stale gray when the box moves or
        // shrinks.
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        reduce_sub_run(&mut chat, &mut life);
        reduce_sub_end(&mut chat, &mut life);
        let grid = flatten(&draw_box(&chat, 60));
        for (r, row) in grid.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                assert!(
                    !(cell.default && cell.style.bg != Color::Default),
                    "cell ({r},{c}) is flagged default but carries a {:?} background",
                    cell.style.bg,
                );
            }
        }
    }

    #[test]
    fn running_box_shows_spinner_and_latest_activity() {
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        reduce_sub_run(&mut chat, &mut life);
        let entry = box_entry(&chat);
        assert_eq!(entry.status, SubAgentStatus::Running);
        // The bash tool start is the sub's last live activity.
        assert_eq!(entry.latest_activity.as_deref(), Some("bash"));

        let r = rows(&draw_box(&chat, 60));
        // The title is `{spinner} agent {N} · {task}`; the glyph is a
        // wall-clock frame, so we only assert it is one of the frame set.
        assert!(r[1].ends_with("agent 0 · check the build setup"), "{r:?}");
        let glyph = r[1].trim_start().chars().next().expect("glyph").to_string();
        assert!(
            SPINNER_FRAMES.contains(&glyph.as_str()),
            "title glyph is a spinner frame: {r:?}",
        );
        assert_eq!(r[2], "", "blank row separates header and body");
        assert_eq!(r[3], " bash", "the body is the latest-activity line");
    }

    #[test]
    fn done_box_shows_check_and_report() {
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        reduce_sub_run(&mut chat, &mut life);
        reduce_sub_end(&mut chat, &mut life);
        assert_eq!(box_entry(&chat).status, SubAgentStatus::Done);
        let r = rows(&draw_box(&chat, 60));
        let body = r.join("\n");
        assert_eq!(r[1], " ✓ agent 0 · check the build setup");
        assert!(body.contains("all good"), "the report is shown: {r:?}");
        // The box renders the report only, never the child transcript.
        assert!(!body.contains("On it."), "no child assistant text: {r:?}");
        assert!(!body.contains("echo hi"), "no child tool body: {r:?}");
    }

    #[test]
    fn long_report_renders_in_full_softwrapped() {
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        reduce_sub_run(&mut chat, &mut life);
        // A long report with distinctive markers at start, middle, and end.
        // The box wraps it in full: no tail-window hint, and the markers land
        // on different rows, so the whole report renders across rows.
        let report = "ALPHA lorem ipsum dolor sit amet consectetur adipiscing elit \
            sed do BETA eiusmod tempor incididunt ut labore et dolore magna aliqua \
            ut enim ad minim OMEGA";
        let _ = reduce(
            &mut chat,
            &mut life,
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(0),
                report: report.into(),
                conclusion: aj_agent::events::SubAgentConclusion::Completed,
            },
            None,
        );
        let _ = reduce(
            &mut chat,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(0),
                messages: Vec::new(),
            },
            None,
        );
        let r = rows(&draw_box(&chat, 40));
        let alpha = r.iter().position(|l| l.contains("ALPHA")).expect("ALPHA");
        let omega = r.iter().position(|l| l.contains("OMEGA")).expect("OMEGA");
        assert!(r.iter().any(|l| l.contains("BETA")), "middle shown: {r:?}");
        assert!(omega > alpha, "the report wraps across rows: {r:?}");
        assert!(
            !r.join("\n").contains("earlier lines"),
            "no window hint remains: {r:?}",
        );
    }

    #[test]
    fn long_report_folds_when_collapsed_and_expands_with_the_toggle() {
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        reduce_sub_run(&mut chat, &mut life);
        // One short token per source line, so the fold counts source lines
        // and each token stays intact after markdown reflows the paragraph.
        let report = (1..=15)
            .map(|i| format!("row{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = reduce(
            &mut chat,
            &mut life,
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(0),
                report,
                conclusion: aj_agent::events::SubAgentConclusion::Completed,
            },
            None,
        );
        let _ = reduce(
            &mut chat,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(0),
                messages: Vec::new(),
            },
            None,
        );

        // Collapsed: the first REPORT_COLLAPSED_LINES lines plus a fold hint,
        // the rest hidden.
        let collapsed = rows(&draw_box(&chat, 60)).join("\n");
        assert!(collapsed.contains("row10"), "head line shown: {collapsed}");
        assert!(
            !collapsed.contains("row11"),
            "folded line hidden: {collapsed}",
        );
        assert!(
            collapsed.contains("5 more lines") && collapsed.contains("expand"),
            "fold hint shown: {collapsed}",
        );

        // Expanded: the whole report, no fold hint.
        let expanded = rows(&draw_box_with(&chat, 60, true)).join("\n");
        assert!(expanded.contains("row15"), "tail line shown: {expanded}");
        assert!(!expanded.contains("more lines"), "no fold hint: {expanded}");
    }

    #[test]
    fn a_report_at_the_collapse_cap_does_not_fold() {
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        let report = (1..=REPORT_COLLAPSED_LINES)
            .map(|i| format!("row{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        finish_with_report(&mut chat, &mut life, &report);
        let collapsed = rows(&draw_box(&chat, 60)).join("\n");
        assert!(
            collapsed.contains(&format!("row{REPORT_COLLAPSED_LINES:02}")),
            "the cap-th line shows: {collapsed}",
        );
        assert!(
            !collapsed.contains("more lines"),
            "a report at the cap does not fold: {collapsed}",
        );
    }

    #[test]
    fn a_trailing_newline_is_popped_before_counting() {
        // Eleven content lines plus a trailing newline: the pop drops the
        // empty twelfth line, so the fold hides one line, not two.
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        let mut report = (1..=11)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        report.push('\n');
        finish_with_report(&mut chat, &mut life, &report);
        let collapsed = rows(&draw_box(&chat, 60)).join("\n");
        assert!(
            collapsed.contains("1 more lines"),
            "the trailing empty line is not counted: {collapsed}",
        );
    }

    #[test]
    fn folding_counts_source_lines_not_wrapped_rows() {
        // Six source lines, each wide enough to soft-wrap to several rows at
        // this width. Six is under the cap, so nothing folds even though the
        // rendered height runs well past it.
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        let wide = "word ".repeat(20);
        let report = vec![wide; 6].join("\n");
        finish_with_report(&mut chat, &mut life, &report);
        let rendered = rows(&draw_box(&chat, 30));
        assert!(
            rendered.len() > REPORT_COLLAPSED_LINES,
            "the report wraps past the cap in rows: {}",
            rendered.len(),
        );
        assert!(
            !rendered.join("\n").contains("more lines"),
            "the fold counts source lines, so it does not trigger: {rendered:?}",
        );
    }

    #[test]
    fn a_report_with_control_bytes_draws_without_panicking() {
        // A bare carriage return underflows the wrap engine unless the report
        // is sanitized first, the way tool cells sanitize their bodies.
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        finish_with_report(&mut chat, &mut life, "done\rok\x1b[0m tail");
        let out = rows(&draw_box(&chat, 60)).join("\n");
        assert!(!out.contains('\u{1b}'), "the escape is stripped: {out:?}");
        assert!(out.contains("done"), "the content shows: {out:?}");
    }

    #[test]
    fn report_renders_as_markdown_not_plain_text() {
        // The report goes through the shared markdown renderer, so inline
        // markup is parsed (its markers consumed and the text styled) rather
        // than shown literally. A plain-text render would keep the `**` and
        // backticks and never set the bold bit.
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        finish_with_report(&mut chat, &mut life, "**ZZZ** and `qqq`");
        let surface = draw_box(&chat, 60);
        let out = rows(&surface).join("\n");
        assert!(!out.contains("**"), "bold markers consumed: {out:?}");
        assert!(!out.contains('`'), "code markers consumed: {out:?}");
        assert!(
            out.contains("ZZZ") && out.contains("qqq"),
            "the text itself is kept: {out:?}",
        );
        // The `ZZZ` span carries the bold emphasis bit, proving the markdown
        // mapper ran (a plain-text render would leave it unset). Bold is a
        // style flag, so this is theme-independent. `Z` and `q` don't appear
        // in the title, so the first such cell is the body span.
        let bold_cell = flatten(&surface)
            .into_iter()
            .flatten()
            .find(|c| c.char.grapheme() == "Z")
            .expect("a cell rendering the bold span");
        assert!(
            bold_cell.style.bold,
            "the bold span is styled bold: {out:?}"
        );
    }

    #[test]
    fn transcript_view_folds_the_box_and_expands_on_toggle() {
        use std::cell::RefCell;
        use std::rc::Rc;

        use crate::transcript::TranscriptView;

        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        let report = (1..=15)
            .map(|i| format!("row{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        finish_with_report(&mut chat, &mut life, &report);
        let chat = Rc::new(RefCell::new(chat));
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);
        let mut view = TranscriptView::new(
            Rc::clone(&chat),
            &theme,
            Rc::new(std::cell::Cell::new(false)),
            Rc::new(std::cell::RefCell::new(None)),
            Rc::new(std::cell::Cell::new(None)),
            Rc::new(std::cell::RefCell::new(
                crate::image_store::ImageStore::default(),
            )),
        );
        let ctx = DrawContext {
            max: MaxSize {
                width: Some(60),
                height: Some(24),
            },
            ..draw_ctx(60, Some(24))
        };

        // Collapsed: the fold hint shows and the tail line is hidden. This is
        // the real draw path (`build_entry_widget` passes `tools_expanded`),
        // not the unit `draw_box` helper.
        let collapsed = rows(&view.draw(&ctx)).join("\n");
        assert!(
            collapsed.contains("more lines"),
            "folded through the view: {collapsed}",
        );
        assert!(
            !collapsed.contains("row15"),
            "the tail is hidden when collapsed: {collapsed}",
        );

        // Toggling the shared flag clears the render cache, so the next draw
        // rebuilds the box in full.
        chat.borrow_mut().tools_expanded = true;
        let expanded = rows(&view.draw(&ctx)).join("\n");
        assert!(
            expanded.contains("row15"),
            "the tail shows when expanded: {expanded}",
        );
        assert!(
            !expanded.contains("more lines"),
            "no hint when expanded: {expanded}",
        );
    }

    #[test]
    fn truncated_and_failed_boxes_show_distinct_glyphs() {
        // A truncated run shows a warning glyph, a failed one an error
        // glyph, both distinct from the clean-`Done` check. Each still
        // renders its report body.
        for (conclusion, glyph) in [
            (SubAgentConclusion::Truncated, "⚠"),
            (SubAgentConclusion::Failed, "✗"),
        ] {
            let mut chat = chat();
            let mut life = AgentLifecycle::default();
            reduce_sub_run(&mut chat, &mut life);
            let _ = reduce(
                &mut chat,
                &mut life,
                AgentEvent::SubAgentEnd {
                    parent: AgentId::Main,
                    child: AgentId::Sub(0),
                    report: "outcome text".into(),
                    conclusion,
                },
                None,
            );
            let r = rows(&draw_box(&chat, 60));
            assert!(
                r[1].starts_with(&format!(" {glyph} agent 0")),
                "{conclusion:?} shows its glyph: {r:?}",
            );
            assert!(
                r.join("\n").contains("outcome text"),
                "{conclusion:?} still renders its report: {r:?}",
            );
        }
    }

    #[test]
    fn long_task_title_truncates_to_one_row_with_ellipsis() {
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        let long_task = "Search the codebase for patterns where we iterate over chat \
            messages and call setters to update rendering settings across every \
            transcript entry"
            .to_string();
        let _ = reduce(
            &mut chat,
            &mut life,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(0),
                task: long_task,
                background: false,
                settings: AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "off".into(),
                    thinking_display: "default".into(),
                    speed: "standard".into(),
                    verbosity: "default".into(),
                },
            },
            None,
        );
        let width = 40;
        let surface = draw_box(&chat, width);
        let r = rows(&surface);
        assert!(r[1].contains("agent 0 · Search"), "{r:?}");
        assert!(r[1].ends_with('…'), "truncated with ellipsis: {r:?}");
        assert!(
            !r[2].contains("transcript entry"),
            "title stays on one row: {r:?}",
        );
        assert_eq!(usize::from(surface.size.width), usize::from(width));
    }

    #[test]
    fn degenerate_width_degrades_to_plain_rendering() {
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        reduce_sub_run(&mut chat, &mut life);
        let surface = draw_box(&chat, 2);
        assert_eq!(surface.size.width, 2);
        // No tinted cells in the plain fallback.
        assert!(
            flatten(&surface)
                .iter()
                .flatten()
                .all(|c| c.style.bg == Color::Default)
        );
    }

    /// The box renders through the transcript view's builder path,
    /// inside a full draw over the populated model.
    #[test]
    fn box_renders_through_the_transcript_view() {
        use std::cell::RefCell;
        use std::rc::Rc;

        use crate::transcript::TranscriptView;

        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        reduce_sub_run(&mut chat, &mut life);
        reduce_sub_end(&mut chat, &mut life);
        let chat = Rc::new(RefCell::new(chat));
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);
        let mut view = TranscriptView::new(
            Rc::clone(&chat),
            &theme,
            std::rc::Rc::new(std::cell::Cell::new(false)),
            std::rc::Rc::new(std::cell::RefCell::new(None)),
            std::rc::Rc::new(std::cell::Cell::new(None)),
            std::rc::Rc::new(std::cell::RefCell::new(
                crate::image_store::ImageStore::default(),
            )),
        );
        let ctx = DrawContext {
            max: MaxSize {
                width: Some(60),
                height: Some(24),
            },
            ..draw_ctx(60, Some(24))
        };
        let surface = view.draw(&ctx);
        let body = rows(&surface).join("\n");
        assert!(body.contains("✓ agent 0 · check the build setup"), "{body}");
        assert!(body.contains("all good"), "the report shows: {body}");
    }
}
