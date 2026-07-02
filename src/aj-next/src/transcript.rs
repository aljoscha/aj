//! The transcript view: a scrolling `ListView` over the active
//! transcript's entries.
//!
//! One list item per [`Entry`], built on demand from the shared
//! [`ChatState`] via `Source::Builder`, so a long transcript only
//! materializes the visible rows each frame. Rendering is the phase-6
//! slice: plain wrapped text per entry kind (markdown parity, tool
//! cells, and sub-agent boxes come with the component phase).

use std::cell::RefCell;
use std::rc::Rc;

use aj_app::chat::{ChatState, Entry, EntryKind, NoticeLevel};
use aj_app::theme::{Theme, ThemeColor, ThemeRgb};
use aj_models::types::AssistantContent;
use vaxis::cell::{Color, Style};
use vaxis::mouse;
use vaxis::vxfw::{
    Builder, DrawContext, Event, EventContext, ListView, RelativePoint, RichText, Source,
    SubSurface, Surface, TextSpan, Widget, WidgetRef,
};

/// Pre-resolved vaxis styles for the transcript's row kinds.
///
/// Resolved once from the theme at construction. The theme's
/// `ColorMode` (truecolor vs 256-color downsampling) is not applied
/// yet, we hand the raw palette values to the terminal. Full theming
/// including downsampling is a later phase.
struct TranscriptStyles {
    text: Style,
    user: Style,
    thinking: Style,
    dim: Style,
    warning: Style,
    error: Style,
}

impl TranscriptStyles {
    fn from_theme(theme: &Theme) -> TranscriptStyles {
        let fg = |token: ThemeColor| Style {
            fg: vaxis_color(theme.fg_color(token)),
            ..Style::default()
        };
        TranscriptStyles {
            text: fg(ThemeColor::Text),
            user: fg(ThemeColor::UserMessageText),
            thinking: Style {
                italic: true,
                ..fg(ThemeColor::ThinkingText)
            },
            dim: fg(ThemeColor::Dim),
            warning: fg(ThemeColor::Warning),
            error: fg(ThemeColor::Error),
        }
    }
}

/// Map a theme palette value onto a vaxis color.
fn vaxis_color(rgb: ThemeRgb) -> Color {
    match rgb {
        ThemeRgb::Rgb(r, g, b) => Color::Rgb([r, g, b]),
        ThemeRgb::Ansi256(i) => Color::Index(i),
        ThemeRgb::Default => Color::Default,
    }
}

/// Lazily builds one row widget per transcript entry of the active
/// view. Shared with the [`ListView`] it feeds.
struct EntryBuilder {
    chat: Rc<RefCell<ChatState>>,
    styles: Rc<TranscriptStyles>,
}

impl Builder for EntryBuilder {
    fn item_at_idx(&self, idx: usize, _cursor: usize) -> Option<WidgetRef> {
        let chat = self.chat.borrow();
        let entry = chat.transcript(chat.active_view())?.entries().get(idx)?;
        let spans = entry_spans(entry, chat.hide_thinking_block, &self.styles);
        Some(Rc::new(RefCell::new(RichText::new(spans))))
    }
}

/// Build the styled spans for one entry, ending in a blank spacer row
/// so consecutive entries don't visually collide.
fn entry_spans(entry: &Entry, hide_thinking: bool, styles: &TranscriptStyles) -> Vec<TextSpan> {
    let span = |text: String, style: Style| TextSpan {
        text,
        style,
        ..TextSpan::default()
    };
    let mut spans = match &entry.kind {
        EntryKind::User(u) => vec![span(format!("> {}", u.joined_text()), styles.user)],
        EntryKind::Assistant(a) => {
            let mut spans = Vec::new();
            for block in &a.message.content {
                let block_span = match block {
                    AssistantContent::Text(t) => span(t.text.clone(), styles.text),
                    AssistantContent::Thinking(t) if t.redacted => {
                        span(format!("[Redacted thinking: {}]", t.thinking), styles.dim)
                    }
                    AssistantContent::Thinking(_) if hide_thinking => {
                        span("Thinking…".to_string(), styles.thinking)
                    }
                    AssistantContent::Thinking(t) => {
                        span(format!("Thinking: {}", t.thinking), styles.thinking)
                    }
                    // Tool calls render as their own `Tool` transcript
                    // entries, so the inline block would duplicate them.
                    AssistantContent::ToolCall(_) => continue,
                };
                if !spans.is_empty() {
                    spans.push(span("\n\n".to_string(), styles.text));
                }
                spans.push(block_span);
            }
            spans
        }
        // Phase-7 placeholders: tool cells, sub-agent boxes, and
        // compaction rows get real widgets with the component phase.
        EntryKind::Tool(t) => vec![span(format!("[tool: {}]", t.tool), styles.dim)],
        EntryKind::SubAgent(s) => vec![span(format!("[sub-agent {}]", s.child), styles.dim)],
        EntryKind::Compaction(_) => vec![span("[compaction]".to_string(), styles.dim)],
        EntryKind::Notice(n) => {
            let style = match n.level {
                NoticeLevel::Info => styles.dim,
                NoticeLevel::Warning => styles.warning,
                NoticeLevel::Error => styles.error,
            };
            vec![span(n.text.clone(), style)]
        }
        EntryKind::TurnUsage(u) => vec![span(u.line(), styles.dim)],
    };
    // Normalize away trailing newlines so the spacer below yields
    // exactly one blank row regardless of how the content ends.
    if let Some(last) = spans.last_mut() {
        let trimmed = last.text.trim_end_matches('\n').len();
        last.text.truncate(trimmed);
    }
    // A trailing "\n\n" adds one empty hard line, which the wrap
    // engine renders as a blank spacer row.
    spans.push(span("\n\n".to_string(), styles.text));
    spans
}

/// The chat area: a follow-tail `ListView` over the active transcript.
///
/// The widget owns the list directly (rather than as a child
/// `WidgetRef`) so mouse events hit-test to this widget and are
/// forwarded, letting follow-tail observe the wheel before the list
/// consumes it.
pub struct TranscriptView {
    chat: Rc<RefCell<ChatState>>,
    list: ListView,
    /// While true, every draw pins the viewport to the bottom so a
    /// streaming turn stays in view. Wheel-up disengages, a scroll
    /// that lands back at the bottom re-engages (Spec E section 1).
    follow_tail: bool,
}

impl TranscriptView {
    pub fn new(chat: Rc<RefCell<ChatState>>, theme: &Theme) -> TranscriptView {
        let builder = EntryBuilder {
            chat: Rc::clone(&chat),
            styles: Rc::new(TranscriptStyles::from_theme(theme)),
        };
        let mut list = ListView::new(Source::Builder(Box::new(builder)));
        // Free-scroll mode: no item cursor while the editor owns the
        // keyboard. Transcript-focus mode arrives in a later phase.
        list.draw_cursor = false;
        TranscriptView {
            chat,
            list,
            follow_tail: true,
        }
    }
}

impl Widget for TranscriptView {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // A flex parent's measuring pass draws children under an
        // unbounded height. The transcript has no inherent height (its
        // flex share decides it), so report zero and skip the list
        // layout entirely, it needs a bounded viewport.
        if ctx.max.height.is_none() {
            return Surface::with_size(vaxis::vxfw::Size {
                width: ctx.max.width.unwrap_or(0),
                height: 0,
            });
        }
        let count = {
            let chat = self.chat.borrow();
            chat.transcript(chat.active_view())
                .map(|t| t.entries().len())
                .unwrap_or(0)
        };
        // The builder has no inherent end-of-list knowledge worth
        // walking for, so refresh the exact count every draw. It also
        // makes `scroll_to_bottom` cheap (no builder walk).
        self.list.item_count = Some(u32::try_from(count).expect("entry count fits u32"));
        if self.follow_tail {
            self.list.scroll_to_bottom();
        }
        let list_surface = self.list.draw(ctx);
        // The draw reconciled any pending wheel scroll, so "we are at
        // the bottom" is now accurate. Landing there re-engages
        // follow-tail.
        if self.list.is_at_bottom() {
            self.follow_tail = true;
        }
        // Wrap the list in an opaque full-size surface: the list draws
        // no background of its own (draw_cursor off), and without one
        // stale cells from the previous frame would survive a scroll.
        let mut surface = Surface::with_size(ctx.max.size());
        surface.children.push(SubSurface {
            origin: RelativePoint { col: 0, row: 0 },
            surface: list_surface,
            z_index: 0,
        });
        surface
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        if let Event::Mouse(m) = event {
            // Any manual upward scroll means the user wants to read
            // history: stop yanking the viewport to the bottom.
            if m.button == mouse::Button::WheelUp {
                self.follow_tail = false;
            }
            self.list.handle_event(ctx, event);
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use aj_app::chat::{AssistantEntry, EntryId, NoticeEntry, Transcript, UserEntry};
    use aj_models::types::{
        AssistantContent, AssistantMessage, StopReason, TextContent, ThinkingContent, UserContent,
    };
    use vaxis::vxfw::{MaxSize, Size};

    use super::*;

    fn styles() -> TranscriptStyles {
        TranscriptStyles::from_theme(&Theme::bundled_dark_with_mode(
            aj_app::theme::ColorMode::Truecolor,
        ))
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

    fn assistant_message(content: Vec<AssistantContent>) -> AssistantMessage {
        AssistantMessage {
            content,
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

    /// Wrap `kind` in a one-entry transcript, since entries are only
    /// mintable through `Transcript::append`.
    fn transcript_with(kind: EntryKind) -> Transcript {
        let mut t = Transcript::default();
        let _: EntryId = t.append(kind);
        t
    }

    fn joined(spans: &[TextSpan]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn user_entry_renders_prefixed_text_with_spacer() {
        let t = transcript_with(EntryKind::User(UserEntry {
            content: vec![UserContent::text("hello")],
            collapsible: false,
        }));
        let spans = entry_spans(&t.entries()[0], false, &styles());
        assert_eq!(joined(&spans), "> hello\n\n");
    }

    #[test]
    fn assistant_entry_renders_blocks_in_order_and_skips_tool_calls() {
        let t = transcript_with(EntryKind::Assistant(AssistantEntry {
            message: assistant_message(vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "pondering".into(),
                    thinking_signature: None,
                    redacted: false,
                }),
                AssistantContent::Text(TextContent {
                    text: "answer\n".into(),
                    text_signature: None,
                }),
            ]),
            finalized: true,
        }));
        let spans = entry_spans(&t.entries()[0], false, &styles());
        // Trailing newline of the last block is normalized so the
        // spacer contributes exactly one blank row.
        assert_eq!(joined(&spans), "Thinking: pondering\n\nanswer\n\n");
    }

    #[test]
    fn hidden_thinking_renders_placeholder() {
        let t = transcript_with(EntryKind::Assistant(AssistantEntry {
            message: assistant_message(vec![AssistantContent::Thinking(ThinkingContent {
                thinking: "secret".into(),
                thinking_signature: None,
                redacted: false,
            })]),
            finalized: true,
        }));
        let spans = entry_spans(&t.entries()[0], true, &styles());
        assert_eq!(joined(&spans), "Thinking…\n\n");
    }

    #[test]
    fn redacted_thinking_renders_marker_even_when_expanded() {
        let t = transcript_with(EntryKind::Assistant(AssistantEntry {
            message: assistant_message(vec![AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: Some("opaque".into()),
                redacted: true,
            })]),
            finalized: true,
        }));
        let spans = entry_spans(&t.entries()[0], false, &styles());
        assert_eq!(joined(&spans), "[Redacted thinking: ]\n\n");
    }

    #[test]
    fn notice_levels_pick_their_style() {
        let s = styles();
        for (level, style) in [
            (NoticeLevel::Info, s.dim),
            (NoticeLevel::Warning, s.warning),
            (NoticeLevel::Error, s.error),
        ] {
            let t = transcript_with(EntryKind::Notice(NoticeEntry {
                level,
                text: "note".into(),
            }));
            let spans = entry_spans(&t.entries()[0], false, &s);
            assert_eq!(spans[0].style, style);
        }
    }

    /// A full draw over a populated model must not panic and must pin
    /// the tail while follow-tail is engaged.
    #[test]
    fn draw_renders_bottom_of_a_long_transcript() {
        use aj_agent::events::AgentSettings;
        use std::sync::Arc;

        let mut chat = ChatState::new(
            AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            0,
            Arc::new(Vec::new()),
        );
        let mut lifecycle = aj_app::session::AgentLifecycle::default();
        for i in 0..50 {
            let _ = aj_app::chat::reduce(
                &mut chat,
                &mut lifecycle,
                aj_agent::events::AgentEvent::Notice {
                    agent_id: aj_agent::events::AgentId::Main,
                    text: format!("row {i}"),
                },
            );
        }
        let chat = Rc::new(RefCell::new(chat));
        let theme = Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor);
        let mut view = TranscriptView::new(Rc::clone(&chat), &theme);

        let surface = view.draw(&draw_ctx(40, 10));
        assert_eq!(surface.size.height, 10);
        assert!(view.follow_tail, "short draw at bottom keeps follow-tail");
        // The last visible child is the final entry (spacer included).
        assert!(view.list.is_at_bottom());
    }
}
