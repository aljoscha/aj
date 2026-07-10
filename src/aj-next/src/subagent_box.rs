//! The sub-agent box: the parent-transcript widget for one sub-agent
//! run, rendering the child's transcript inline.
//!
//! This is `aj`'s compact mode: a gray box with a one-line
//! `{glyph} agent {N} · {task}` title, the child transcript laid out
//! below it through the same per-entry builders the top-level list
//! uses (tool entries compose header-only via their `header_only`
//! flag), tail-windowed to [`COMPACT_ROWS`] rows with an
//! `… (N earlier lines)` hint so a long-running sub-agent never
//! floods the scrollback.
//!
//! `aj`'s full ("observing") mode has no counterpart here: switching
//! the observed agent swaps the [`TranscriptView`]'s whole list over
//! to that agent's transcript (`ChatState::set_active_view`), so the
//! box itself never renders full-size.
//!
//! Like the other transcript widgets, the box is built fresh per draw
//! from data extracted out of [`ChatState`] at build time. Drawing
//! needs no model access, so the `ListView` builder's shared borrow
//! never nests or escapes.
//!
//! [`TranscriptView`]: crate::transcript::TranscriptView

use aj_agent::events::AgentId;
use aj_app::chat::{ChatState, SubAgentEntry, SubAgentStatus};
use vaxis::cell::{Cell, Color, Style};
use vaxis::vxfw::{
    DrawContext, MaxSize, Overflow, RichText, Size, SubSurface, Surface, TextSpan, Widget,
};

use crate::bubble::{MIN_BUBBLE_WIDTH, PADDING_X, PADDING_Y};
use crate::transcript::{TranscriptStyles, build_entry_widget};

/// Inner transcript rows shown in the compact box before the tail
/// window kicks in (hint row included), the single knob to tune.
const COMPACT_ROWS: usize = 18;

/// On-screen representation of one sub-agent run, boxed inside the
/// parent's transcript.
pub(crate) struct SubAgentBox {
    /// The `{glyph} agent {N} · {task}` title spans, one logical line
    /// (the task text is whitespace-normalized at build time).
    title: Vec<TextSpan>,
    /// The child transcript's entry widgets, in append order. Built
    /// at construction so drawing needs no `ChatState` access.
    children: Vec<Box<dyn Widget>>,
    /// The box tint (the shared pending-tool gray, matching `aj`).
    bg: Color,
    /// Style of the `… (N earlier lines)` window hint.
    dim: Style,
}

/// Build the box for `entry`, snapshotting the child transcript's
/// entries into per-entry widgets.
///
/// Sub-agents never spawn sub-agents (the `agent` tool is excluded
/// from a child's inherited tool list), so the inner entries are
/// built non-recursively: an impossible nested `SubAgent` entry
/// degrades to the builder's dim stub line.
pub(crate) fn build_subagent_box(
    entry: &SubAgentEntry,
    chat: &ChatState,
    styles: &TranscriptStyles,
) -> SubAgentBox {
    let span = |text: String, style: Style| TextSpan {
        text,
        style,
        ..TextSpan::default()
    };
    let glyph = match entry.status {
        SubAgentStatus::Running => span("▸".into(), styles.dim),
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
    let children = chat
        .transcript(AgentId::Sub(entry.child))
        .map(|t| {
            t.entries()
                .iter()
                .map(|e| build_entry_widget(e, chat, styles, true, None).into_boxed())
                .collect()
        })
        .unwrap_or_default();
    SubAgentBox {
        title,
        children,
        bg: styles.tool_pending_bg,
        dim: styles.dim,
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

/// Whether a composited row is visually empty: no text and no
/// background tint. Bg-painted pad rows of an inner bubble count as
/// content, so trimming can't eat a bubble's frame.
fn row_is_blank(row: &[Cell]) -> bool {
    row.iter()
        .all(|c| c.char.grapheme().trim().is_empty() && c.style.bg == Color::Default)
}

impl SubAgentBox {
    /// Lay the child widgets out at `inner_ctx`'s width and composite
    /// them into one row list, in entry order. Trailing blank rows
    /// (the last entry's spacer) are trimmed so the box's own bottom
    /// padding provides the closing rhythm.
    fn body_rows(&mut self, inner_ctx: &DrawContext) -> Vec<Vec<Cell>> {
        let mut rows = Vec::new();
        for child in &mut self.children {
            rows.extend(surface_rows(&child.draw(inner_ctx)));
        }
        while rows.last().is_some_and(|r| row_is_blank(r)) {
            rows.pop();
        }
        rows
    }

    /// Tail-window `rows` to [`COMPACT_ROWS`], reserving one row for
    /// the dropped-lines hint so the total body never exceeds the
    /// window.
    fn window_tail(&self, mut rows: Vec<Vec<Cell>>, inner_ctx: &DrawContext) -> Vec<Vec<Cell>> {
        if rows.len() <= COMPACT_ROWS {
            return rows;
        }
        let keep = COMPACT_ROWS - 1;
        let earlier = rows.len() - keep;
        let tail = rows.split_off(rows.len() - keep);
        let mut hint = RichText::new(vec![TextSpan {
            text: format!("… ({earlier} earlier lines)"),
            style: self.dim,
            ..TextSpan::default()
        }]);
        hint.softwrap = false;
        let mut windowed = surface_rows(&hint.draw(inner_ctx));
        windowed.extend(tail);
        windowed
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
        content.extend(self.window_tail(body, &inner_ctx));

        if !bubble {
            return Self::draw_plain(content, width);
        }

        // The bubble frame: bg-filled padding around the content rows
        // plus one untinted spacer row, the same rhythm as `Bubble`.
        // Content is written directly into the buffer (not as child
        // surfaces) because the tail window slices rows out of the
        // middle of the children's output.
        let content_height = u16::try_from(content.len()).expect("windowed box rows fit u16");
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

    use aj_agent::events::{AgentEvent, AgentSettings};
    use aj_agent::message::AgentMessage;
    use aj_agent::tool::ToolDetails;
    use aj_app::chat::{EntryKind, reduce};
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
    /// task prompt, an assistant line, and one bash tool call. The
    /// active view stays `Main`, so the tool entry is flagged
    /// `header_only` by the reducer.
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
        let mut b = build_subagent_box(box_entry(chat), chat, &s);
        b.draw(&draw_ctx(width, None))
    }

    #[test]
    fn compact_box_renders_title_content_and_tint() {
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        reduce_sub_run(&mut chat, &mut life);
        let s = styles();
        let surface = draw_box(&chat, 60);
        let r = rows(&surface);

        // Frame: bg-painted blank pads around the content, untinted
        // spacer at the bottom.
        assert_eq!(r[0], "", "top pad row is blank");
        assert_eq!(r[1], " ▸ agent 0 · check the build setup");
        assert_eq!(r.last().unwrap(), "", "spacer row is blank");
        // Inner entries composite in order: task bubble, assistant
        // text, header-only tool line.
        let body = r.join("\n");
        assert!(body.contains("check the build setup"), "{r:?}");
        assert!(body.contains("On it."), "{r:?}");
        assert!(
            body.contains("✓ bash(command=\"echo hi\")"),
            "header-only tool line inside: {r:?}",
        );
        assert!(
            !body.contains("$ echo hi"),
            "header-only tools show no body: {r:?}",
        );

        // Tint: every bubble row is painted (the inner user bubble
        // keeps its own tint), the trailing spacer row is not.
        let grid = flatten(&surface);
        let h = grid.len();
        for (row_idx, row) in grid.iter().enumerate().take(h - 1) {
            for (col_idx, cell) in row.iter().enumerate() {
                assert!(
                    cell.style.bg == s.tool_pending_bg || cell.style.bg == s.user_message_bg,
                    "cell ({row_idx},{col_idx}) untinted",
                );
            }
        }
        assert!(grid[h - 1].iter().all(|c| c.style.bg == Color::Default));
    }

    #[test]
    fn inner_user_bubble_keeps_its_own_tint() {
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        reduce_sub_run(&mut chat, &mut life);
        let s = styles();
        let grid = flatten(&draw_box(&chat, 60));
        assert!(
            grid.iter()
                .flatten()
                .any(|c| c.style.bg == s.user_message_bg),
            "the task prompt's user bubble tint survives the box paint",
        );
    }

    #[test]
    fn box_never_tints_a_default_cell() {
        // A tinted cell must not stay flagged `default`. The render diff's
        // default fast-path treats two `default` cells as equal regardless of
        // background, so a `default` cell carrying the box tint reads as blank
        // and is never repainted, stranding stale gray when the box moves or
        // shrinks. The inner user bubble's blank spacer row is the cell that
        // used to slip through the box paint.
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        reduce_sub_run(&mut chat, &mut life);
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
    fn done_status_flips_glyph_to_check() {
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        reduce_sub_run(&mut chat, &mut life);
        reduce_sub_end(&mut chat, &mut life);
        assert_eq!(box_entry(&chat).status, SubAgentStatus::Done);
        let r = rows(&draw_box(&chat, 60));
        assert_eq!(r[1], " ✓ agent 0 · check the build setup");
    }

    #[test]
    fn long_inner_transcript_windows_to_the_tail_with_hint() {
        let mut chat = chat();
        let mut life = AgentLifecycle::default();
        reduce_sub_run(&mut chat, &mut life);
        for i in 0..40 {
            let _ = reduce(
                &mut chat,
                &mut life,
                AgentEvent::Notice {
                    agent_id: AgentId::Sub(0),
                    text: format!("inner-{i}-marker"),
                },
            );
        }
        let surface = draw_box(&chat, 60);
        let r = rows(&surface);
        let body = r.join("\n");
        assert!(body.contains("inner-39-marker"), "{r:?}");
        assert!(!body.contains("inner-0-marker"), "{r:?}");
        // The hint carries no expand-key suffix: the box windows
        // unconditionally, there is nothing to expand.
        assert!(body.contains("earlier lines)"), "{r:?}");
        assert!(!body.contains("to expand"), "{r:?}");
        // Total height: title + windowed body + pads + spacer.
        assert_eq!(
            usize::from(surface.size.height),
            1 + COMPACT_ROWS + 2 * usize::from(PADDING_Y) + 1,
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
        let surface = draw_box(&chat, width);
        let r = rows(&surface);
        assert!(r[1].starts_with(" ▸ agent 0 · Search"), "{r:?}");
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
                .all(|c| c.style.bg == Color::Default || c.style.bg == styles().user_message_bg)
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
    }
}
