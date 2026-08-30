//! The live footer: one dim row of session facts under the editor.
//!
//! Reads the shared [`ChatState`] (model line, context usage, tasks),
//! [`StatusState`] (running sub-agent count), and, for a local run, the
//! session's [`TaskRegistry`] (notices queued for the viewed agent) at draw
//! time, so it refreshes on every frame without any event-driven sync of its
//! own.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::LazyLock;

use aj_agent::TaskRegistry;
use aj_agent::tool::{TaskKind, TaskStatus};
use aj_app::chat::ChatState;
use aj_app::footer::{
    UsageSeverity, context_usage_display, format_agent_activity, format_pending_notices,
};
use vaxis::cell::Style;
use vaxis::vxfw::{DrawContext, Event, EventContext, RichText, Surface, TextSpan, Widget};

use crate::status::StatusState;
use crate::transcript::TranscriptStyles;

/// Display label of the agent-picker chord shown in the activity
/// part, resolved from the shared binding data, so it reflects a user
/// `[keybindings]` override. Cached on first render, which is after the
/// overrides are installed at startup.
static AGENT_PICKER_KEY_LABEL: LazyLock<String> = LazyLock::new(|| {
    aj_app::keybindings::action_shortcut(aj_app::keybindings::ACTION_AGENT_PICKER)
        .expect("aj.agent.open has a default chord")
});

/// The footer row widget.
pub(crate) struct FooterLine {
    chat: Rc<RefCell<ChatState>>,
    status: Rc<RefCell<StatusState>>,
    styles: Rc<TranscriptStyles>,
    /// Working-directory display string, fixed for the session.
    cwd: String,
    /// The session's task registry, read for the viewed agent's queued
    /// notices. Session-scoped, so a session switch replaces it
    /// through [`FooterLine::set_task_registry`].
    ///
    /// `None` in connect mode. The count has no protocol equivalent: it is
    /// the host's own undelivered-notice bookkeeping, which no frame and no
    /// read carries, so the part is simply left out rather than guessed at.
    task_registry: Option<TaskRegistry>,
}

impl FooterLine {
    pub(crate) fn new(
        chat: Rc<RefCell<ChatState>>,
        status: Rc<RefCell<StatusState>>,
        styles: Rc<TranscriptStyles>,
        cwd: String,
        task_registry: Option<TaskRegistry>,
    ) -> FooterLine {
        FooterLine {
            chat,
            status,
            styles,
            cwd,
            task_registry,
        }
    }

    /// Point the footer at another session's task registry.
    pub(crate) fn set_task_registry(&mut self, task_registry: Option<TaskRegistry>) {
        self.task_registry = task_registry;
    }

    /// Notices queued for `owner` in the session's registry, zero when there
    /// is no registry to read.
    pub(crate) fn pending_notices(&self, owner: aj_agent::events::AgentId) -> usize {
        self.task_registry
            .as_ref()
            .map_or(0, |registry| registry.pending_notices(owner))
    }

    /// Replace the palette styles, for a runtime theme swap.
    pub(crate) fn set_styles(&mut self, styles: Rc<TranscriptStyles>) {
        self.styles = styles;
    }
}

impl Widget for FooterLine {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let span = |text: String, style: Style| TextSpan {
            text,
            style,
            ..TextSpan::default()
        };
        let dim = self.styles.dim;
        let chat = self.chat.borrow();
        let active = chat.active_view();

        // Each part is a short span list so the context-usage part
        // can color its percentage while everything else stays dim.
        let mut parts: Vec<Vec<TextSpan>> = Vec::new();
        if let Some(model) = chat.footers().model_line(active) {
            parts.push(vec![span(model, dim)]);
        }
        parts.push(vec![span(self.cwd.clone(), dim)]);
        if let Some(usage) = context_usage_display(chat.footers().context_usage(active)) {
            match usage.percent {
                None => parts.push(vec![span(usage.ratio, dim)]),
                Some(pct) => {
                    let pct_style = match usage.severity {
                        UsageSeverity::Critical => self.styles.error,
                        UsageSeverity::Warning => self.styles.warning,
                        UsageSeverity::Normal => dim,
                    };
                    parts.push(vec![
                        span(format!("{} ", usage.ratio), dim),
                        span(pct, pct_style),
                    ]);
                }
            }
        }
        // Activity: running sub-agents plus running background bash
        // tasks. Agent-backed tasks are excluded because their
        // sub-agent is already in the agent count.
        let agents = self.status.borrow().sub_agents_running;
        let tasks = chat
            .tasks()
            .values()
            .filter(|t| matches!(t.kind, TaskKind::Bash { .. }) && t.status == TaskStatus::Running)
            .count();
        if agents + tasks > 0 {
            parts.push(vec![span(
                format_agent_activity(agents, tasks, AGENT_PICKER_KEY_LABEL.as_str()),
                dim,
            )]);
        }
        // Notices queued for the viewed agent: tasks that finished but
        // whose completion the agent has not been handed yet, because a
        // notice can only be delivered between tool batches. Local-only:
        // this is host-internal bookkeeping with no wire representation.
        if let Some(text) = format_pending_notices(self.pending_notices(active)) {
            parts.push(vec![span(text, dim)]);
        }

        // One-column left indent matching the chat scrollback's inset,
        // parts joined with `  ·  `. Softwrap off: a long cwd or model
        // name truncates with an ellipsis instead of wrapping, which
        // would grow the footer and push the editor up a row.
        let mut spans = vec![span(" ".to_string(), dim)];
        for (i, part) in parts.into_iter().enumerate() {
            if i > 0 {
                spans.push(span("  ·  ".to_string(), dim));
            }
            spans.extend(part);
        }
        let mut rich = RichText::new(spans);
        rich.softwrap = false;
        rich.draw(ctx)
    }

    fn handle_event(&mut self, _ctx: &mut EventContext, _event: &Event) {}
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
    use aj_agent::tool::TaskNotice;
    use aj_agent::types::TokenUsage;
    use aj_app::chat::reduce;
    use aj_app::session::AgentLifecycle;
    use aj_app::theme::Theme;

    use super::*;

    fn chat_with_window(window: u64) -> Rc<RefCell<ChatState>> {
        Rc::new(RefCell::new(ChatState::new(
            AgentSettings {
                provider: "anthropic".into(),
                model_id: "opus".into(),
                thinking: "high".into(),
                thinking_display: "default".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            window,
            Arc::new(Vec::new()),
        )))
    }

    fn styles() -> Rc<TranscriptStyles> {
        Rc::new(TranscriptStyles::from_theme(
            &Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor),
            crate::terminal::TerminalCaps::default(),
        ))
    }

    fn footer(chat: Rc<RefCell<ChatState>>, status: StatusState) -> FooterLine {
        footer_with_registry(chat, status, TaskRegistry::default())
    }

    fn footer_with_registry(
        chat: Rc<RefCell<ChatState>>,
        status: StatusState,
        task_registry: TaskRegistry,
    ) -> FooterLine {
        FooterLine::new(
            chat,
            Rc::new(RefCell::new(status)),
            styles(),
            "/home/user/proj".into(),
            Some(task_registry),
        )
    }

    fn notice(owner: AgentId, task_id: usize) -> TaskNotice {
        TaskNotice {
            owner,
            task_id,
            kind: TaskKind::Bash {
                command: "make".into(),
            },
            label: "make".into(),
            status: TaskStatus::Exited(Some(0)),
            body: "exit 0".into(),
        }
    }

    fn usage(tokens: u64) -> TokenUsage {
        TokenUsage {
            accumulated_input: 0,
            turn_input: tokens,
            accumulated_output: 0,
            turn_output: 0,
            accumulated_cache_write: 0,
            turn_cache_write: 0,
            accumulated_cache_read: 0,
            turn_cache_read: 0,
            turn_incomplete: false,
            accumulated_incomplete: false,
        }
    }

    fn draw_rows(f: &mut FooterLine, width: u16) -> Vec<String> {
        let surface = f.draw(&crate::test_support::draw_ctx(width, None));
        crate::test_support::rows(&surface)
    }

    #[test]
    fn footer_joins_model_cwd_and_usage_with_separators() {
        let chat = chat_with_window(200_000);
        chat.borrow_mut()
            .footers_mut()
            .record_turn_usage(AgentId::Main, &usage(12_345));
        let mut f = footer(chat, StatusState::default());
        let r = draw_rows(&mut f, 80);
        assert_eq!(r.len(), 1, "one footer row: {r:?}");
        assert_eq!(r[0], " opus high  ·  /home/user/proj  ·  12k/200k (6.2%)");
    }

    #[test]
    fn footer_renders_an_incomplete_prompt_with_a_marker() {
        let chat = chat_with_window(200_000);
        let mut usage = usage(12_345);
        usage.turn_incomplete = true;
        chat.borrow_mut()
            .footers_mut()
            .record_turn_usage(AgentId::Main, &usage);
        let mut f = footer(chat, StatusState::default());
        let r = draw_rows(&mut f, 80);
        assert_eq!(r, [" opus high  ·  /home/user/proj  ·  ≥12k/200k"]);
    }

    #[test]
    fn footer_renders_unknown_tokens_as_question_mark() {
        let mut f = footer(chat_with_window(200_000), StatusState::default());
        let r = draw_rows(&mut f, 80);
        assert!(r[0].ends_with("?/200k"), "{r:?}");
    }

    #[test]
    fn footer_suppresses_usage_for_zero_window() {
        let chat = chat_with_window(0);
        chat.borrow_mut()
            .footers_mut()
            .record_turn_usage(AgentId::Main, &usage(1_000));
        let mut f = footer(chat, StatusState::default());
        let r = draw_rows(&mut f, 80);
        assert_eq!(r[0], " opus high  ·  /home/user/proj", "{r:?}");
    }

    /// The percentage substring picks up the threshold color; the
    /// surrounding ratio stays dim.
    #[test]
    fn footer_colors_the_percentage_by_occupancy() {
        let s = styles();
        // Column of the opening paren, by grapheme (`·` is multibyte,
        // so a byte offset would overshoot the cell column).
        let pct_col = |text: &str| text.chars().position(|c| c == '(').expect("pct present");
        for (tokens, want) in [(140_001u64, s.warning.fg), (180_001, s.error.fg)] {
            let chat = chat_with_window(200_000);
            chat.borrow_mut()
                .footers_mut()
                .record_turn_usage(AgentId::Main, &usage(tokens));
            let mut f = footer(chat, StatusState::default());
            let surface = f.draw(&crate::test_support::draw_ctx(80, None));
            let grid = crate::test_support::flatten(&surface);
            let text = crate::test_support::rows(&surface)[0].clone();
            let pct_start = pct_col(&text);
            assert_eq!(grid[0][pct_start].style.fg, want, "{text:?}");
            // The ratio right before the percentage stays dim.
            assert_eq!(grid[0][pct_start - 2].style.fg, s.dim.fg, "{text:?}");
        }
        // Low occupancy stays dim.
        let chat = chat_with_window(200_000);
        chat.borrow_mut()
            .footers_mut()
            .record_turn_usage(AgentId::Main, &usage(20_000));
        let mut f = footer(chat, StatusState::default());
        let surface = f.draw(&crate::test_support::draw_ctx(80, None));
        let grid = crate::test_support::flatten(&surface);
        let text = crate::test_support::rows(&surface)[0].clone();
        let pct_start = pct_col(&text);
        assert_eq!(grid[0][pct_start].style.fg, s.dim.fg, "{text:?}");
    }

    /// The activity part counts running sub-agents and running bash
    /// tasks only: agent-kind and finished tasks are filtered out.
    #[test]
    fn footer_counts_agents_and_running_bash_tasks() {
        let chat = chat_with_window(200_000);
        let mut life = AgentLifecycle::default();
        let start = |id: usize, kind: TaskKind, label: &str| AgentEvent::TaskStart {
            agent_id: AgentId::Main,
            task_id: id,
            call_id: format!("tu-{id}"),
            kind,
            label: label.into(),
        };
        {
            let mut c = chat.borrow_mut();
            let bash = |cmd: &str| TaskKind::Bash {
                command: cmd.into(),
            };
            let _ = reduce(
                &mut c,
                &mut life,
                start(1, bash("sleep 5"), "sleep 5"),
                None,
            );
            let _ = reduce(&mut c, &mut life, start(2, bash("make"), "make"), None);
            let _ = reduce(
                &mut c,
                &mut life,
                start(
                    3,
                    TaskKind::Agent {
                        agent_id: 1,
                        task: "sub task".into(),
                    },
                    "sub task",
                ),
                None,
            );
            // Task 2 finished: it drops out of the running count.
            let _ = reduce(
                &mut c,
                &mut life,
                AgentEvent::TaskEnd {
                    agent_id: AgentId::Main,
                    task_id: 2,
                    call_id: "tu-2".into(),
                    status: TaskStatus::Exited(Some(0)),
                    label: "make".into(),
                },
                None,
            );
        }
        let mut f = footer(
            chat,
            StatusState {
                sub_agents_running: 2,
                ..StatusState::default()
            },
        );
        let r = draw_rows(&mut f, 100);
        assert!(r[0].ends_with("2 agents, 1 task (Alt+A)"), "{r:?}");
    }

    #[test]
    fn footer_hides_activity_when_nothing_runs() {
        let mut f = footer(chat_with_window(200_000), StatusState::default());
        let r = draw_rows(&mut f, 80);
        assert!(!r[0].contains("agent ("), "{r:?}");
        assert!(!r[0].contains("task ("), "{r:?}");
    }

    #[test]
    fn footer_hides_notice_part_when_nothing_is_queued() {
        let mut f = footer(chat_with_window(200_000), StatusState::default());
        let r = draw_rows(&mut f, 80);
        assert!(!r[0].contains("pending"), "{r:?}");
    }

    /// Connect mode has no registry to read, so the part is absent rather
    /// than reported as zero or guessed at.
    #[test]
    fn footer_without_a_registry_omits_the_notice_part() {
        let mut f = FooterLine::new(
            chat_with_window(200_000),
            Rc::new(RefCell::new(StatusState::default())),
            styles(),
            "/home/user/proj".into(),
            None,
        );
        assert_eq!(f.pending_notices(AgentId::Main), 0);
        let r = draw_rows(&mut f, 100);
        assert!(!r[0].contains("pending"), "{r:?}");
    }

    #[test]
    fn footer_shows_a_single_pending_notice() {
        let registry = TaskRegistry::default();
        registry.push_notice(notice(AgentId::Main, 1));
        let mut f =
            footer_with_registry(chat_with_window(200_000), StatusState::default(), registry);
        let r = draw_rows(&mut f, 100);
        assert!(r[0].ends_with("1 notice pending"), "{r:?}");
    }

    #[test]
    fn footer_pluralizes_several_pending_notices() {
        let registry = TaskRegistry::default();
        for id in 1..=3 {
            registry.push_notice(notice(AgentId::Main, id));
        }
        let mut f =
            footer_with_registry(chat_with_window(200_000), StatusState::default(), registry);
        let r = draw_rows(&mut f, 100);
        assert!(r[0].ends_with("3 notices pending"), "{r:?}");
    }

    /// The count is scoped to the viewed agent: another agent's queued
    /// notices are that agent's footer's business.
    #[test]
    fn footer_ignores_notices_owned_by_another_agent() {
        let registry = TaskRegistry::default();
        registry.push_notice(notice(AgentId::Sub(1), 1));
        let mut f =
            footer_with_registry(chat_with_window(200_000), StatusState::default(), registry);
        let r = draw_rows(&mut f, 100);
        assert!(!r[0].contains("pending"), "{r:?}");
    }

    /// Softwrap is off: a narrow terminal truncates the row with an
    /// ellipsis instead of wrapping it (which would grow the footer).
    #[test]
    fn footer_truncates_instead_of_wrapping() {
        let chat = chat_with_window(200_000);
        chat.borrow_mut()
            .footers_mut()
            .record_turn_usage(AgentId::Main, &usage(12_345));
        let mut f = footer(chat, StatusState::default());
        let surface = f.draw(&crate::test_support::draw_ctx(20, None));
        assert_eq!(surface.size.height, 1, "never wraps");
        let grid = crate::test_support::flatten(&surface);
        assert_eq!(grid[0][19].char.grapheme(), "…", "ellipsis at the edge");
    }

    /// Empty-model-line degenerate state still renders the cwd (the
    /// part list is never empty, so the footer is never blank).
    #[test]
    fn footer_always_shows_the_cwd() {
        let mut f = footer(chat_with_window(0), StatusState::default());
        let r = draw_rows(&mut f, 80);
        assert!(r[0].contains("/home/user/proj"), "{r:?}");
    }
}
