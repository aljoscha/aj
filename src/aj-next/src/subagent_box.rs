//! The sub-agent box: the parent-transcript widget for one sub-agent
//! run.
//!
//! A gray box with a one-line `{glyph} agent {N} · {task}` title and a
//! metadata body. Once the sub-agent is done the body is its report,
//! soft-wrapped. While it runs the glyph is an event-driven spinner
//! frame (advanced by the box's activity counter) and the body is a
//! single latest-activity line that clips with an ellipsis.
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

use aj_app::chat::{SubAgentEntry, SubAgentStatus};
use vaxis::cell::{Cell, Color, Style};
use vaxis::vxfw::{
    DrawContext, MaxSize, Overflow, RichText, Size, SubSurface, Surface, TextSpan, Widget,
};

use crate::bubble::{MIN_BUBBLE_WIDTH, PADDING_X, PADDING_Y};
use crate::transcript::TranscriptStyles;

/// Spinner frames for a `Running` box's glyph. A local copy of the
/// status-loader frame set: the modules stay decoupled, so the box
/// does not reach into `status.rs`'s private const.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// On-screen representation of one sub-agent run, boxed inside the
/// parent's transcript.
pub(crate) struct SubAgentBox {
    /// The `{glyph} agent {N} · {task}` title spans, one logical line
    /// (the task text is whitespace-normalized at build time). The
    /// glyph is a check when done or a spinner frame while running.
    title: Vec<TextSpan>,
    /// The body spans: the report when done, the latest-activity line
    /// while running. Empty when there is nothing to show (a done
    /// sub-agent with no report, a running sub with no activity yet).
    body: Vec<TextSpan>,
    /// Whether the body soft-wraps. A done report wraps across rows; a
    /// running activity line stays on one row and clips with an ellipsis.
    body_softwrap: bool,
    /// The box tint (the shared pending-tool gray, matching `aj`).
    bg: Color,
}

/// Build the box for `entry` from its metadata: the report (done) or a
/// spinner glyph plus latest-activity line (running). Reads no
/// transcript.
pub(crate) fn build_subagent_box(entry: &SubAgentEntry, styles: &TranscriptStyles) -> SubAgentBox {
    let span = |text: String, style: Style| TextSpan {
        text,
        style,
        ..TextSpan::default()
    };
    let glyph = match entry.status {
        // The running glyph advances on activity, not a wall-clock: the
        // frame is picked by the box's activity counter, so it steps on
        // each sub-agent event without a redraw timer.
        SubAgentStatus::Running => {
            // Modulo the frame count so the index is bounded and fits usize.
            let n = u64::try_from(SPINNER_FRAMES.len()).unwrap_or(u64::MAX);
            let idx = usize::try_from(entry.activity_ticks % n).unwrap_or(0);
            span(SPINNER_FRAMES[idx].into(), styles.dim)
        }
        SubAgentStatus::Done => span("✓".into(), styles.success),
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
    // The body is metadata only. A done box shows its report; a running
    // box shows the latest-activity line. An empty report is a real,
    // accepted case (a sub-agent that concluded on a tool call), and
    // renders a thin title-only box.
    let (body, body_softwrap) = match entry.status {
        SubAgentStatus::Done => match entry.report.as_deref() {
            Some(report) if !report.is_empty() => (vec![span(report.into(), styles.text)], true),
            _ => (Vec::new(), true),
        },
        SubAgentStatus::Running => match entry.latest_activity.as_deref() {
            Some(activity) if !activity.is_empty() => {
                (vec![span(activity.into(), styles.dim)], false)
            }
            _ => (Vec::new(), false),
        },
    };
    SubAgentBox {
        title,
        body,
        body_softwrap,
        bg: styles.tool_pending_bg,
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
    /// Draw the body spans at `inner_ctx`'s width. The report wraps
    /// across rows; a running activity line stays on one row and clips
    /// with an ellipsis. Empty body draws no rows.
    fn body_rows(&self, inner_ctx: &DrawContext) -> Vec<Vec<Cell>> {
        if self.body.is_empty() {
            return Vec::new();
        }
        let mut text = RichText::new(self.body.clone());
        text.softwrap = self.body_softwrap;
        text.overflow = Overflow::Ellipsis;
        surface_rows(&text.draw(inner_ctx))
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
        content.extend(self.body_rows(&inner_ctx));

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

    use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
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
        TranscriptStyles::from_theme(&Theme::bundled_dark_with_mode(ColorMode::Truecolor))
    }

    fn chat() -> ChatState {
        ChatState::new(
            AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
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
            let _ = reduce(chat, life, event);
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
            },
            AgentEvent::AgentEnd {
                agent_id: sub,
                messages: Vec::new(),
            },
        ];
        for event in events {
            let _ = reduce(chat, life, event);
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

    fn draw_box(chat: &ChatState, width: u16) -> Surface {
        let s = styles();
        let mut b = build_subagent_box(box_entry(chat), &s);
        b.draw(&draw_ctx(width, None))
    }

    /// The spinner frame a `Running` box shows for the given activity count.
    fn frame_for(ticks: u64) -> &'static str {
        let n = u64::try_from(SPINNER_FRAMES.len()).unwrap_or(u64::MAX);
        SPINNER_FRAMES[usize::try_from(ticks % n).unwrap_or(0)]
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
        // spacer at the bottom.
        assert_eq!(r[0], "", "top pad row is blank");
        assert_eq!(r[1], " ✓ agent 0 · check the build setup");
        assert_eq!(r[2], " all good", "the report is the body");
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
        let frame = frame_for(entry.activity_ticks);

        let r = rows(&draw_box(&chat, 60));
        assert_eq!(r[1], format!(" {frame} agent 0 · check the build setup"));
        assert_eq!(r[2], " bash", "the body is the latest-activity line");
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
            },
        );
        let _ = reduce(
            &mut chat,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(0),
                messages: Vec::new(),
            },
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
                    speed: "standard".into(),
                    verbosity: "default".into(),
                },
            },
        );
        let width = 40;
        let frame = frame_for(box_entry(&chat).activity_ticks);
        let surface = draw_box(&chat, width);
        let r = rows(&surface);
        assert!(
            r[1].starts_with(&format!(" {frame} agent 0 · Search")),
            "{r:?}",
        );
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
